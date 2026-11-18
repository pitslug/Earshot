//! Voice samples taken back by the user's speaker edits (T12.77b5, docs/voice-samples.md 3.3
//! and 6). Production writes no samples yet; the tests make them with `enroll`. Every
//! name contains `edit`.

use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::voiceprint::{SampleOrigin, enroll};

use super::*;

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("level@1")
}

/// A recognisable 8-value voice (`k` picks the direction).
fn voice(k: usize) -> Vec<f32> {
    (0..8)
        .map(|i| if i == k { 0.9 } else { 0.05 + i as f32 * 0.003 })
        .collect()
}

/// Writes a sample from voice `speaker` of `meeting` into `person`'s print (as a Save will
/// from T12.77b6). Returns the bytes of a run of its vector, to look for on disk.
fn save_sample(
    svc: &Services,
    meeting: EntityId,
    speaker: EntityId,
    person: EntityId,
    k: usize,
) -> Vec<u8> {
    write(svc, |tx, stamp| {
        enroll(
            tx,
            person,
            &[Embedding::new(model(), voice(k))],
            Duration::from_secs(20),
            SampleOrigin::Meeting { meeting, speaker },
            &EnrolmentConfig::default(),
            stamp,
        )
        .map_err(voiceprint_err)?;
        Ok(())
    })
    .unwrap();
    let s = svc.store().samples_from(meeting, speaker).unwrap();
    let s = s.last().unwrap();
    s.vector().unwrap()[2..6]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect()
}

fn samples_from(svc: &Services, meeting: EntityId, speaker: EntityId) -> usize {
    svc.store().samples_from(meeting, speaker).unwrap().len()
}

/// Whether `needle` is in the store's file or its WAL.
fn on_disk(dir: &TempDir, needle: &[u8]) -> bool {
    let db = earshot_store::DataDir::new(dir.0.join("data")).db_path();
    ["", "-wal", "-journal"].iter().any(|suffix| {
        std::fs::read(format!("{}{suffix}", db.display()))
            .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
    })
}

/// A second connection held open across an erasure, so a closing checkpoint cannot hide a
/// missing one.
fn second_connection(dir: &TempDir) -> earshot_store::ReadOnlyStore {
    earshot_store::ReadOnlyStore::open(&earshot_store::DataDir::new(dir.0.join("data")).db_path())
        .unwrap()
}

fn linked(svc: &Services, meeting: EntityId, speaker: EntityId, name: &str) -> EntityId {
    link_speaker(
        svc,
        meeting,
        speaker,
        &PersonChoice::New { name: name.into() },
    )
    .unwrap()
    .person
}

#[test]
fn edit_not_this_person_takes_this_meetings_sample_back_and_nothing_else() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 700);
    let remote = remote_speakers(&svc, m);
    let rowan = linked(&svc, m, remote[0].id, "Rowan");
    let needle = save_sample(&svc, m, remote[0].id, rowan, 1);
    // Another voice of the same person in this meeting keeps its sample.
    force_link(&svc, m, remote[1].id, rowan);
    save_sample(&svc, m, remote[1].id, rowan, 2);
    assert!(on_disk(&dir, &needle));
    let _other = second_connection(&dir);

    let done = not_this_person(&svc, m, remote[0].id).unwrap();
    assert_eq!(done.person, rowan);
    assert_eq!(
        done.took_back,
        TakenBack {
            taken_back: 1,
            checkpoint_pending: false
        }
    );
    assert!(!done.saved_before_samples);
    assert_eq!(samples_from(&svc, m, remote[0].id), 0);
    assert_eq!(samples_from(&svc, m, remote[1].id), 1);
    assert!(
        !on_disk(&dir, &needle),
        "the vector leaves the file and the WAL"
    );
}

#[test]
fn edit_not_this_person_accepts_a_voice_with_a_sample_and_no_identity_entry() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    // A plain meeting: speakers with no identity record at all.
    let m = meeting(&svc, "Planning", 701);
    let alice = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.role != SpeakerRole::Me)
        .unwrap();
    let person = linked(&svc, m, alice.id, "Alice Invented");
    let row = speaker(&svc, m, "Alice Invented");
    assert!(identities_in(&svc.store(), m).unwrap().is_empty());
    assert!(!can_not_this_person(&**svc.store(), &row, &[]).unwrap());
    assert_eq!(
        refused(not_this_person(&svc, m, alice.id)),
        EditError::NoStoredVoice(alice.id)
    );
    save_sample(&svc, m, alice.id, person, 3);
    assert!(can_not_this_person(&**svc.store(), &row, &[]).unwrap());
    let done = not_this_person(&svc, m, alice.id).unwrap();
    assert_eq!(done.took_back.taken_back, 1);
    let after = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.id == alice.id)
        .unwrap();
    assert_eq!(after.person_id, None);
    assert!(after.display_name.starts_with("Speaker "));
    assert!(
        identities_in(&svc.store(), m).unwrap().is_empty(),
        "no ignore recorded"
    );
}

#[test]
fn edit_linking_a_voice_to_another_person_takes_its_sample_back_and_the_same_keeps_it() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 702);
    let s = remote_speakers(&svc, m)[0].id;
    let first = linked(&svc, m, s, "Sasha Invented");
    let needle = save_sample(&svc, m, s, first, 4);
    // The same person again (a merged duplicate of them too): the sample stays.
    let again = link_speaker(&svc, m, s, &PersonChoice::Existing(first)).unwrap();
    assert_eq!(again.took_back.taken_back, 0);
    let dup = new_person(&svc, "S. Invented", false, 77);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        merge_persons(store, dup, first, hlc, &*clock).unwrap();
    }
    assert_eq!(
        link_speaker(&svc, m, s, &PersonChoice::Existing(dup))
            .unwrap()
            .took_back
            .taken_back,
        0
    );
    assert_eq!(samples_from(&svc, m, s), 1);
    let _other = second_connection(&dir);
    // Someone else: the voice stops being theirs.
    let moved = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Robin Invented".into(),
        },
    )
    .unwrap();
    assert_eq!(moved.took_back.taken_back, 1);
    assert_eq!(samples_from(&svc, m, s), 0);
    assert!(!on_disk(&dir, &needle));
}

#[test]
fn edit_merge_takes_the_merged_away_voices_samples_back() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 703);
    let remote = remote_speakers(&svc, m);
    let p = linked(&svc, m, remote[0].id, "Kim Invented");
    force_link(&svc, m, remote[1].id, p);
    save_sample(&svc, m, remote[0].id, p, 5);
    let needle = save_sample(&svc, m, remote[1].id, p, 6);
    let _other = second_connection(&dir);
    let merged = merge_speakers(&svc, m, remote[0].id, remote[1].id).unwrap();
    assert_eq!(merged.samples_taken_back, 1);
    assert!(!merged.checkpoint_pending);
    assert_eq!(
        samples_from(&svc, m, remote[0].id),
        1,
        "the kept voice keeps its own"
    );
    assert_eq!(samples_from(&svc, m, remote[1].id), 0);
    assert!(!on_disk(&dir, &needle));
}

/// What a Save made before samples existed left (docs/voice-samples.md 5.2): the voice's
/// centroid in the person's print base, and the frozen marker on this computer's entry.
fn saved_before_samples_existed(
    svc: &Services,
    meeting: EntityId,
    speaker: EntityId,
    person: EntityId,
) {
    let entry = identities_in(&svc.store(), meeting)
        .unwrap()
        .into_iter()
        .find(|i| i.speaker == speaker)
        .unwrap();
    write(svc, |tx, stamp| {
        let id = EntityId::generate(stamp.clock, &mut *stamp.rng);
        let meta = earshot_core::identity::SyncMeta::new(stamp.hlc, stamp.clock);
        let unit = earshot_core::voiceprint::unit_vector(&entry.centroid.vector).unwrap();
        let base = earshot_core::voiceprint::Voiceprint::from_stored(
            id,
            meta,
            person,
            entry.centroid.model.clone(),
            unit,
            1,
            20_000,
        );
        tx.put_voiceprint(base).map_err(voiceprint_err)?;
        update_identity_entry(tx, meeting, speaker, |i| i.enrolled = true)?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn edit_not_this_person_says_saved_before_samples_only_while_the_print_has_its_base() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 704);
    let s = remote_speakers(&svc, m)[0].id;
    let morgan = linked(&svc, m, s, "Morgan Invented");
    saved_before_samples_existed(&svc, m, s, morgan);
    let done = not_this_person(&svc, m, s).unwrap();
    assert!(done.saved_before_samples, "{done:?}");
    assert_eq!(done.took_back.taken_back, 0);

    let m2 = processed(&svc, 705);
    let s2 = remote_speakers(&svc, m2)[0].id;
    let avery = linked(&svc, m2, s2, "Avery Invented");
    saved_before_samples_existed(&svc, m2, s2, avery);
    let saved2 = SavedVoice {
        person: avery,
        remembered: Remembered::AlreadySaved,
        took_back: TakenBack::default(),
        moved_from: None,
    };
    // Forgotten on another computer: the print's tombstone arrives (here, the core erasure
    // alone), and this computer's identity entry keeps its marker.
    write(&svc, |tx, stamp| {
        earshot_core::voiceprint::forget_voiceprints(tx, saved2.person, stamp.hlc, stamp.clock)
            .map_err(voiceprint_err)?;
        Ok(())
    })
    .unwrap();
    assert!(
        identities_in(&svc.store(), m2)
            .unwrap()
            .iter()
            .any(|i| i.speaker == s2 && i.enrolled)
    );
    // A voice saved for them later, elsewhere, as a sample: a print with no base.
    let m3 = processed(&svc, 706);
    let s3 = remote_speakers(&svc, m3)[0].id;
    force_link(&svc, m3, s3, saved2.person);
    save_sample(&svc, m3, s3, saved2.person, 1);
    let done = not_this_person(&svc, m2, s2).unwrap();
    assert!(
        !done.saved_before_samples,
        "the marker alone does not count once the print lost its base"
    );
}

/// Gives `speaker`'s identity entry a new centroid of `k` (as a new final pass would).
fn new_centroid(svc: &Services, meeting: EntityId, speaker: EntityId, k: usize) {
    write(svc, |tx, _| {
        update_identity_entry(tx, meeting, speaker, |i| {
            let n = i.centroid.vector.len();
            let mut v = vec![0.05_f32; n];
            v[k % n] = 0.9;
            i.centroid = Embedding::new(i.centroid.model.clone(), v);
        })?;
        Ok(())
    })
    .unwrap();
}

fn sample_ids(svc: &Services, meeting: EntityId, speaker: EntityId) -> Vec<EntityId> {
    svc.store()
        .samples_from(meeting, speaker)
        .unwrap()
        .iter()
        .map(earshot_core::voiceprint::VoiceprintSample::id)
        .collect()
}

fn save(svc: &Services, m: EntityId, s: EntityId, choice: PersonChoice, any: bool) -> SavedVoice {
    save_voice_with(svc, m, s, &choice, &calibrated(any)).unwrap()
}

fn new_name(name: &str) -> PersonChoice {
    PersonChoice::New { name: name.into() }
}

/// T12.77b6 (docs/voice-samples.md 5.3): a repeat Save writes nothing; a new centroid
/// replaces this meeting's sample; one sample per voice per meeting per person.
#[test]
fn edit_save_again_writes_nothing_and_a_new_centroid_replaces_the_sample() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 720);
    let s = remote_speakers(&svc, m)[0].id;
    let first = save(&svc, m, s, new_name("Quinn Invented"), true);
    assert!(matches!(first.remembered, Remembered::Enrolled { .. }));
    let ids = sample_ids(&svc, m, s);
    assert_eq!(ids.len(), 1);
    // The same voice, saved again for the same person: nothing written.
    let again = save(&svc, m, s, PersonChoice::Existing(first.person), true);
    assert_eq!(again.remembered, Remembered::AlreadySaved);
    assert_eq!(again.took_back.taken_back, 0);
    assert_eq!(sample_ids(&svc, m, s), ids);
    // A new centroid for the voice: the old sample is replaced, never counted twice.
    new_centroid(&svc, m, s, 4);
    let replaced = save(&svc, m, s, PersonChoice::Existing(first.person), true);
    assert!(
        matches!(replaced.remembered, Remembered::Replaced { person, .. } if person == first.person)
    );
    assert_eq!(replaced.took_back.taken_back, 1);
    let now = sample_ids(&svc, m, s);
    assert_eq!(now.len(), 1);
    assert_ne!(now, ids);
    assert_eq!(print_count(&svc, first.person), Some(1));
}

/// 5.3 step 2: a refusal (too little speech for what the print would be without the old
/// sample, or an uncalibrated model) takes nothing back.
#[test]
fn edit_a_refused_resave_leaves_the_earlier_sample_counting() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 721);
    let s = remote_speakers(&svc, m)[0].id;
    let first = save(&svc, m, s, new_name("Riley Invented"), true);
    let ids = sample_ids(&svc, m, s);
    new_centroid(&svc, m, s, 5);
    let short = save(&svc, m, s, PersonChoice::Existing(first.person), false);
    assert!(
        matches!(short.remembered, Remembered::NotEnoughSpeech { .. }),
        "{short:?}"
    );
    assert_eq!(sample_ids(&svc, m, s), ids, "the earlier sample stays");
    let uncalibrated = save_voice(&svc, m, s, &PersonChoice::Existing(first.person)).unwrap();
    assert!(matches!(
        uncalibrated.remembered,
        Remembered::Uncalibrated { .. }
    ));
    assert_eq!(sample_ids(&svc, m, s), ids);
    assert_eq!(print_count(&svc, first.person), Some(1));
}

/// 5.3 step 1: a voice saved before samples existed (the frozen marker and a print with a
/// base) answers AlreadySaved and writes nothing; once the print lost its base it saves anew.
#[test]
fn edit_a_voice_saved_before_samples_is_already_saved_until_the_base_is_gone() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 722);
    let s = remote_speakers(&svc, m)[0].id;
    let sam = linked(&svc, m, s, "Sam Invented");
    saved_before_samples_existed(&svc, m, s, sam);
    let r = save(&svc, m, s, PersonChoice::Existing(sam), true);
    assert_eq!(r.remembered, Remembered::AlreadySaved);
    assert!(sample_ids(&svc, m, s).is_empty());
    // Forgotten elsewhere (the print erased, this computer's marker kept): saved anew.
    write(&svc, |tx, stamp| {
        earshot_core::voiceprint::forget_voiceprints(tx, sam, stamp.hlc, stamp.clock)
            .map_err(voiceprint_err)?;
        Ok(())
    })
    .unwrap();
    let r = save(&svc, m, s, PersonChoice::Existing(sam), true);
    assert!(matches!(r.remembered, Remembered::Enrolled { .. }), "{r:?}");
    assert_eq!(sample_ids(&svc, m, s).len(), 1);
}

/// 5.3: Save with a new person judges the voice against them in the same transaction: a
/// refusal keeps the person and the link and saves nothing.
#[test]
fn edit_save_with_a_new_person_refused_keeps_the_person_and_the_link() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 723);
    let s = remote_speakers(&svc, m)[0].id;
    let r = save(&svc, m, s, new_name("Toby Invented"), false);
    assert!(
        matches!(r.remembered, Remembered::NotEnoughSpeech { .. }),
        "{r:?}"
    );
    assert!(list_persons(&svc).unwrap().iter().any(|p| p.id == r.person));
    let row = view(&svc, m)
        .speakers
        .into_iter()
        .find(|x| x.id == s)
        .unwrap();
    assert_eq!(row.person_id, Some(r.person));
    assert!(sample_ids(&svc, m, s).is_empty());
    assert_eq!(r.moved_from, None);
}

/// 6.2: Save on a voice saved for someone else moves its sample: X loses exactly it, Y gains
/// one; a refusal for Y still takes X's back and saves nothing; an error rolls everything back.
#[test]
fn edit_save_on_a_wrongly_named_voice_moves_its_sample() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 724);
    let remote = remote_speakers(&svc, m);
    let s = remote[0].id;
    // Accepted: X to Y.
    let x = save(&svc, m, s, new_name("Uma Invented"), true).person;
    let moved = save(&svc, m, s, new_name("Vic Invented"), true);
    assert!(
        matches!(moved.remembered, Remembered::Enrolled { person, .. } if person == moved.person)
    );
    assert_eq!(moved.moved_from, Some(x));
    assert_eq!(moved.took_back.taken_back, 1);
    assert_eq!(print_count(&svc, x), Some(0));
    assert_eq!(print_count(&svc, moved.person), Some(1));
    // Refused for the new person: the link moves and X's sample goes, nothing is saved.
    let s2 = remote[1].id;
    let x2 = save(&svc, m, s2, new_name("Wren Invented"), true).person;
    let refused = save(&svc, m, s2, new_name("Xan Invented"), false);
    assert!(
        matches!(refused.remembered, Remembered::NotEnoughSpeech { .. }),
        "{refused:?}"
    );
    assert_eq!(refused.moved_from, None);
    assert_eq!(refused.took_back.taken_back, 1);
    assert_eq!(print_count(&svc, x2), Some(0));
    assert_eq!(print_count(&svc, refused.person), None);
    // An error in the enrolment (the new person's print of another dimension): nothing of
    // the save stands, X's sample is live and the row still X's.
    let y = new_person(&svc, "Yael Invented", false, 725);
    write(&svc, |tx, stamp| {
        let id = EntityId::generate(stamp.clock, &mut *stamp.rng);
        let meta = earshot_core::identity::SyncMeta::new(stamp.hlc, stamp.clock);
        let odd = earshot_core::voiceprint::Voiceprint::from_stored(
            id,
            meta,
            y,
            model(),
            vec![1.0, 0.0, 0.0],
            1,
            20_000,
        );
        tx.put_voiceprint(odd).map_err(voiceprint_err)?;
        Ok(())
    })
    .unwrap();
    let before = sample_ids(&svc, m, s);
    assert!(save_voice_with(&svc, m, s, &PersonChoice::Existing(y), &calibrated(true)).is_err());
    assert_eq!(sample_ids(&svc, m, s), before);
    let row = view(&svc, m)
        .speakers
        .into_iter()
        .find(|r| r.id == s)
        .unwrap();
    assert_eq!(row.person_id, Some(moved.person));
}

/// T12.77b5 review (1): "saved before samples" is judged before the take back: a voice with
/// the marker AND a live sample from its origin is saved as a sample, not before.
#[test]
fn edit_not_this_person_with_the_marker_and_a_sample_takes_the_sample_back() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 726);
    let s = remote_speakers(&svc, m)[0].id;
    let p = save(&svc, m, s, new_name("Zoe Invented"), true).person;
    write(&svc, |tx, _| {
        update_identity_entry(tx, m, s, |i| i.enrolled = true)?;
        Ok(())
    })
    .unwrap();
    let done = not_this_person(&svc, m, s).unwrap();
    assert!(!done.saved_before_samples, "{done:?}");
    assert_eq!(done.took_back.taken_back, 1);
    assert_eq!(print_count(&svc, p), Some(0));
}

/// T12.77b5 review (3): relinking a voice to the person it is linked to takes back a stray
/// sample of the voice saved for someone else.
#[test]
fn edit_an_unchanged_link_takes_back_a_stray_sample_for_someone_else() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 727);
    let s = remote_speakers(&svc, m)[0].id;
    let x = linked(&svc, m, s, "Ash Invented");
    let other = new_person(&svc, "Bay Invented", false, 728);
    save_sample(&svc, m, s, other, 2);
    let again = link_speaker(&svc, m, s, &PersonChoice::Existing(x)).unwrap();
    assert_eq!(again.took_back.taken_back, 1);
    assert!(sample_ids(&svc, m, s).is_empty());
}

/// T12.77b5 review (4): a reader holding a snapshot blocks the checkpoint after a take back;
/// "Not" reports it pending, never as an error, and the erasure is committed.
#[test]
fn edit_not_this_person_reports_a_checkpoint_another_connection_blocks() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 729);
    let s = remote_speakers(&svc, m)[0].id;
    save(&svc, m, s, new_name("Cal Invented"), true);
    let path = svc.data.db_path();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let mut other = earshot_store::Store::open(&path).unwrap();
        other
            .transaction(|tx| -> Result<(), earshot_store::StoreError> {
                tx.meeting(m)?;
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
    });
    ready_rx.recv().unwrap();
    let done = not_this_person(&svc, m, s).unwrap();
    assert_eq!(done.took_back.taken_back, 1);
    assert!(done.took_back.checkpoint_pending, "{done:?}");
    release_tx.send(()).unwrap();
    reader.join().unwrap();
    assert!(
        sample_ids(&svc, m, s).is_empty(),
        "the take back is committed"
    );
}

/// T12.77b6 review (6): a voice saved before samples existed and then re-transcribed keeps
/// its frozen marker (the final pass carries it to the same voice), so saving it again counts
/// it nothing twice.
#[test]
fn edit_a_retranscribed_voice_saved_before_samples_is_not_counted_twice() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 730);
    let s = remote_speakers(&svc, m)[0].id;
    let dee = linked(&svc, m, s, "Dee Invented");
    saved_before_samples_existed(&svc, m, s, dee);
    let cancel = CancelToken::new();
    retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let carried = identities_in(&svc.store(), m)
        .unwrap()
        .into_iter()
        .find(|i| i.speaker == s)
        .unwrap();
    assert!(carried.enrolled, "the marker is carried to the same voice");
    let again = save(&svc, m, s, PersonChoice::Existing(dee), true);
    assert_eq!(again.remembered, Remembered::AlreadySaved);
    assert!(sample_ids(&svc, m, s).is_empty());
    assert_eq!(print_count(&svc, dee), Some(1), "only the base");
}
