//! Voice samples taken back by the user's speaker edits (T12.77b5, docs/voice-samples.md 3.3
//! and 6). Production writes no samples yet; the tests make them with `enroll_sample`. Every
//! name contains `edit`.

use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::voiceprint::{SampleOrigin, enroll_sample};

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
        enroll_sample(
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

#[test]
fn edit_not_this_person_says_saved_before_samples_only_while_the_print_has_its_base() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 704);
    let s = remote_speakers(&svc, m)[0].id;
    // Saved before samples existed: today's Save adds to the print's base and sets the frozen
    // marker.
    let saved = save_voice_with(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Morgan Invented".into(),
        },
        &calibrated(true),
    )
    .unwrap();
    assert!(matches!(saved.remembered, Remembered::Enrolled { .. }));
    let done = not_this_person(&svc, m, s).unwrap();
    assert!(done.saved_before_samples, "{done:?}");
    assert_eq!(done.took_back.taken_back, 0);

    let m2 = processed(&svc, 705);
    let s2 = remote_speakers(&svc, m2)[0].id;
    let saved2 = save_voice_with(
        &svc,
        m2,
        s2,
        &PersonChoice::New {
            name: "Avery Invented".into(),
        },
        &calibrated(true),
    )
    .unwrap();
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
