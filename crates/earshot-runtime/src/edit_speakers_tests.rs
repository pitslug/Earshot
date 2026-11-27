//! Saving and ignoring a voice from the Speakers panel (T12.19). Every name contains `edit`.

use super::*;

fn entry(
    svc: &Services,
    meeting: EntityId,
    speaker: EntityId,
) -> crate::identities::SpeakerIdentity {
    identities_in(&svc.store(), meeting)
        .unwrap()
        .into_iter()
        .find(|i| i.speaker == speaker)
        .unwrap_or_else(|| panic!("no identity entry for {speaker}"))
}

fn row(svc: &Services, meeting: EntityId, id: EntityId) -> Speaker {
    view(svc, meeting)
        .speakers
        .into_iter()
        .find(|s| s.id == id)
        .unwrap()
}

fn ignored(svc: &Services, meeting: EntityId, id: EntityId) -> bool {
    let legacy = legacy_ignores(&svc.store(), meeting).unwrap();
    is_ignored(&row(svc, meeting, id), &legacy)
}

/// Whether legacy entry ignores still count here (`crate::ignore_copy`).
fn legacy_on(svc: &Services) -> bool {
    !crate::ignore_copy::copied(&svc.store()).unwrap()
}

// T12.93 changed this test's pins: the ignore is now the synced row's own flag (the row is
// written and re-stamped, the entry's flag stays false), and the entry's `given_stamp` follows
// the ignore so ignore then un-ignore reads as the pass's naming by stamp.
#[test]
fn edit_ignore_speaker_keeps_the_label_and_is_carried_by_a_retranscribe() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 500);
    let remote = remote_speakers(&svc, m);
    let s = remote[0].clone();
    assert!(can_ignore(&s));
    assert!(!is_ignored(&s, &legacy_ignores(&svc.store(), m).unwrap()));

    assert!(ignore_speaker(&svc, m, s.id).unwrap());
    assert!(
        !ignore_speaker(&svc, m, s.id).unwrap(),
        "a repeat writes nothing"
    );
    assert!(ignored(&svc, m, s.id));
    // The row carries the choice (synced) and keeps its label and person; the entry's flag is
    // never written, and its stamp follows the row so it reads as an edit only by the flag.
    let after = row(&svc, m, s.id);
    assert!(after.ignored);
    assert_eq!(after.display_name, s.display_name);
    assert_eq!(after.person_id, None);
    assert_ne!(
        after.meta.updated_at, s.meta.updated_at,
        "re-stamped, so it syncs"
    );
    let e = entry(&svc, m, s.id);
    assert!(!e.ignored && !e.user_edited);
    assert_eq!(e.given_stamp, Some(after.meta.updated_at));
    assert!(e.edited(&after, legacy_on(&svc)));
    // Never enrolled: an ignored voice has nobody to be remembered for.
    assert_eq!(
        refused(remember_voices_with(&svc, m, &[s.id], &calibrated(true))),
        EditError::NoPerson(s.id)
    );

    // A re-transcription continues the voice as the user's edit: same row, same label,
    // still ignored, still nobody, and the outcome says so.
    let cancel = CancelToken::new();
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let HostPost::Done(outcome) = again.post else {
        panic!("{:?}", again.post);
    };
    let carried = outcome.speakers.iter().find(|p| p.id == s.id).unwrap();
    assert!(carried.ignored && carried.person.is_none());
    assert!(
        outcome
            .speakers
            .iter()
            .filter(|p| p.id != s.id)
            .all(|p| !p.ignored)
    );
    assert_eq!(row(&svc, m, s.id).display_name, s.display_name);
    assert!(row(&svc, m, s.id).ignored);
    assert!(
        !entry(&svc, m, s.id).ignored,
        "the pass never writes the entry flag"
    );

    // Un-ignored: it needs a name again and reads as the pass's naming, even though the
    // re-run carried the ignore as an edit.
    assert!(unignore_speaker(&svc, m, s.id).unwrap());
    assert!(!unignore_speaker(&svc, m, s.id).unwrap());
    assert!(!ignored(&svc, m, s.id));
    let back = row(&svc, m, s.id);
    assert!(!back.ignored);
    let e = entry(&svc, m, s.id);
    assert!(!e.ignored);
    assert!(!e.edited(&back, legacy_on(&svc)), "{e:?}");
}

/// T12.93 (3a): ignore then un-ignore without a re-run reads as the pass's own naming again
/// by stamp, so a later run may still renumber or name the voice.
#[test]
fn edit_ignore_then_unignore_reads_as_the_pass_naming_again() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 501);
    let s = remote_speakers(&svc, m)[0].clone();
    assert!(!entry(&svc, m, s.id).edited(&s, legacy_on(&svc)));
    assert!(ignore_speaker(&svc, m, s.id).unwrap());
    assert!(unignore_speaker(&svc, m, s.id).unwrap());
    let back = row(&svc, m, s.id);
    let e = entry(&svc, m, s.id);
    assert_eq!(e.given_stamp, Some(back.meta.updated_at));
    assert!(!e.user_edited && !e.edited(&back, legacy_on(&svc)), "{e:?}");
}

/// T12.93 (5a): before the one-off copy runs, a 0.4.2 entry flag is still an ignore: the
/// rule, a re-run (which moves it onto the row) and un-ignore (which clears both) honour it.
#[test]
fn edit_a_legacy_entry_ignore_counts_until_the_copy() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 502);
    let remote = remote_speakers(&svc, m);
    let (a, b) = (remote[0].clone(), remote[1].clone());
    // What 0.4.2 wrote: the entry flag, the row untouched.
    for s in [&a, &b] {
        write(&svc, |tx, _| {
            update_identity_entry(tx, m, s.id, |i| i.ignored = true).map(|_| ())
        })
        .unwrap();
    }
    assert!(legacy_on(&svc));
    assert!(ignored(&svc, m, a.id) && !row(&svc, m, a.id).ignored);
    assert!(
        !ignore_speaker(&svc, m, a.id).unwrap(),
        "already ignored, nothing written"
    );
    // A re-transcription honours it and carries it into the new entry, never onto the row
    // (a re-stamp there could beat a remote name not pulled yet).
    let a_before = row(&svc, m, a.id);
    let cancel = CancelToken::new();
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let HostPost::Done(outcome) = again.post else {
        panic!("{:?}", again.post);
    };
    assert!(
        outcome
            .speakers
            .iter()
            .find(|p| p.id == a.id)
            .unwrap()
            .ignored
    );
    let a_after = row(&svc, m, a.id);
    assert!(
        !a_after.ignored && !row(&svc, m, b.id).ignored,
        "rows untouched"
    );
    assert_eq!(
        a_after.meta.updated_at, a_before.meta.updated_at,
        "no re-stamp"
    );
    assert!(entry(&svc, m, a.id).ignored, "carried for the copy");
    assert!(ignored(&svc, m, a.id) && ignored(&svc, m, b.id));
    // Un-ignore clears a legacy entry flag (the row was never flagged).
    assert!(unignore_speaker(&svc, m, b.id).unwrap());
    assert!(!ignored(&svc, m, b.id) && !entry(&svc, m, b.id).ignored);
    // The copy puts the carried flag on the row; after it the entry flag is never read.
    crate::ignore_copy::copy_once(&svc).unwrap();
    assert!(!legacy_on(&svc));
    assert!(row(&svc, m, a.id).ignored);
    let mut clear = row(&svc, m, a.id);
    clear.ignored = false;
    clear.meta.updated_at.logical += 1;
    svc.store().put_speaker(clear).unwrap();
    assert!(entry(&svc, m, a.id).ignored);
    assert!(
        !ignored(&svc, m, a.id),
        "the entry flag is not read after the copy"
    );
    // A re-run after the copy writes no flag into the entry.
    let cancel = CancelToken::new();
    retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert!(!entry(&svc, m, a.id).ignored);
}

#[test]
fn edit_unignoring_after_a_retranscribe_keeps_a_rename_the_user_made() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 505);
    let s = remote_speakers(&svc, m)[1].id;
    assert!(rename_speaker(&svc, m, s, "Visitor").unwrap());
    assert!(ignore_speaker(&svc, m, s).unwrap());
    let cancel = CancelToken::new();
    retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert!(ignored(&svc, m, s));
    assert!(unignore_speaker(&svc, m, s).unwrap());
    // The row is the user's own edit: un-ignoring does not undo it.
    let after = row(&svc, m, s);
    assert_eq!(after.display_name, "Visitor");
    assert!(entry(&svc, m, s).edited(&after, legacy_on(&svc)));
}

// T12.93 changed this test: a voice the final pass kept nothing of can now be ignored (the
// choice is on its row), so the old `NoStoredVoice` refusal became an acceptance.
#[test]
fn edit_ignore_refuses_the_user_and_a_named_voice_and_takes_a_voice_with_nothing_stored() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 510);
    let me = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();
    assert_eq!(
        refused(ignore_speaker(&svc, m, me.id)),
        EditError::NotUnnamed(me.id)
    );
    let s = remote_speakers(&svc, m)[0].id;
    link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Rowan".into(),
        },
    )
    .unwrap();
    assert_eq!(
        refused(ignore_speaker(&svc, m, s)),
        EditError::NotUnnamed(s)
    );
    assert!(!can_ignore(&row(&svc, m, s)));
    assert!(!can_ignore(&me));
    // A voice the final pass kept nothing of: offered and accepted.
    let quiet = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Speaker 9",
        "hello there",
        9_000,
        511,
    );
    assert!(can_ignore(&row(&svc, m, quiet)));
    assert!(ignore_speaker(&svc, m, quiet).unwrap());
    assert!(ignored(&svc, m, quiet));
    let nobody = EntityId::from_bytes([5; 16]);
    assert_eq!(
        refused(ignore_speaker(&svc, m, nobody)),
        EditError::SpeakerNotFound(nobody)
    );
    assert_eq!(
        refused(unignore_speaker(&svc, m, nobody)),
        EditError::SpeakerNotFound(nobody)
    );
}

/// The one rule (T12.93): a flag on "Me" or on a linked row (a pulled row is a trust boundary)
/// never makes it ignored.
#[test]
fn edit_is_ignored_needs_an_unnamed_voice_that_is_not_the_user() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 512);
    let none = BTreeSet::new();
    let mut me = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();
    me.ignored = true;
    assert!(!is_ignored(&me, &none));
    let mut linked = remote_speakers(&svc, m)[0].clone();
    linked.ignored = true;
    linked.person_id = Some(EntityId::from_bytes([6; 16]));
    assert!(!is_ignored(&linked, &none));
    linked.person_id = None;
    assert!(is_ignored(&linked, &none));
}

#[test]
fn edit_naming_an_ignored_voice_ends_the_ignore() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 520);
    let remote = remote_speakers(&svc, m);
    assert!(ignore_speaker(&svc, m, remote[0].id).unwrap());
    assert!(ignore_speaker(&svc, m, remote[1].id).unwrap());
    link_speaker(
        &svc,
        m,
        remote[0].id,
        &PersonChoice::New {
            name: "Avery".into(),
        },
    )
    .unwrap();
    assert!(!entry(&svc, m, remote[0].id).ignored && !row(&svc, m, remote[0].id).ignored);
    save_voice_with(
        &svc,
        m,
        remote[1].id,
        &PersonChoice::New {
            name: "Blair".into(),
        },
        &calibrated(true),
    )
    .unwrap();
    assert!(!entry(&svc, m, remote[1].id).ignored && !row(&svc, m, remote[1].id).ignored);
}

#[test]
fn edit_ignored_voice_stays_unlinked_when_a_voiceprint_would_name_it() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    // Two meetings processed before any voiceprint exists (the level fakes give every
    // meeting the same voices; the fakes' voiceprint threshold is calibrated): in one the
    // far end's first voice is ignored, the other is the control.
    let m = processed(&svc, 532);
    let voice = remote_speakers(&svc, m)[0].clone();
    assert!(ignore_speaker(&svc, m, voice.id).unwrap());
    let control = processed(&svc, 531);
    // Then a voiceprint of that voice is saved in a third meeting.
    let elsewhere = processed(&svc, 530);
    let avery = save_voice_with(
        &svc,
        elsewhere,
        remote_speakers(&svc, elsewhere)[0].id,
        &PersonChoice::New {
            name: "Avery".into(),
        },
        &calibrated(true),
    )
    .unwrap()
    .person;
    assert!(print_count(&svc, avery).is_some());
    let cancel = CancelToken::new();

    // Control: a re-run names the voice by that voiceprint.
    assert!(
        view(&svc, control)
            .speakers
            .iter()
            .all(|s| s.person_id != Some(avery))
    );
    retranscribe_with(&svc, control, &cancel, fake_post, &mut |_| {}).unwrap();
    assert!(
        view(&svc, control)
            .speakers
            .iter()
            .any(|s| s.person_id == Some(avery)),
        "the voiceprint names the voice on a re-run"
    );

    // The ignored voice: the re-run keeps it unlinked and ignored.
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let HostPost::Done(outcome) = again.post else {
        panic!("{:?}", again.post);
    };
    let carried = outcome.speakers.iter().find(|p| p.id == voice.id).unwrap();
    assert!(carried.ignored && carried.person.is_none(), "{carried:?}");
    let after = row(&svc, m, voice.id);
    assert_eq!(after.person_id, None);
    assert_eq!(after.display_name, voice.display_name);
    assert!(ignored(&svc, m, voice.id));
    assert!(
        view(&svc, m)
            .speakers
            .iter()
            .all(|s| s.person_id != Some(avery)),
        "no other row took the voiceprint's name"
    );
}

#[test]
fn edit_save_voice_links_and_remembers_in_one_step() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 530);
    let remote = remote_speakers(&svc, m);
    let any = calibrated(true);

    let saved = save_voice_with(
        &svc,
        m,
        remote[0].id,
        &PersonChoice::New {
            name: " Morgan ".into(),
        },
        &any,
    )
    .unwrap();
    let after = row(&svc, m, remote[0].id);
    assert_eq!(after.person_id, Some(saved.person));
    assert_eq!(after.display_name, "Morgan");
    let Remembered::Enrolled { person, .. } = saved.remembered else {
        panic!("{saved:?}");
    };
    assert_eq!(person, saved.person);
    assert_eq!(print_count(&svc, saved.person), Some(1));
    assert!(
        !entry(&svc, m, remote[0].id).enrolled,
        "nothing writes the frozen marker"
    );
    let saved_samples = svc.store().samples_from(m, remote[0].id).unwrap();
    assert_eq!(
        saved_samples.len(),
        1,
        "saved as this meeting voice's sample"
    );
    // Saved again to the same person: linked already, remembered already.
    let again = save_voice_with(
        &svc,
        m,
        remote[0].id,
        &PersonChoice::Existing(saved.person),
        &any,
    )
    .unwrap();
    assert_eq!(again.remembered, Remembered::AlreadySaved);
    assert_eq!(print_count(&svc, saved.person), Some(1));

    // The production thresholds: the level fakes' model is uncalibrated, so the voice is
    // named and linked but no voiceprint is stored, and the result says so.
    let other = save_voice(
        &svc,
        m,
        remote[1].id,
        &PersonChoice::New {
            name: "Quinn".into(),
        },
    )
    .unwrap();
    assert!(
        matches!(other.remembered, Remembered::Uncalibrated { .. }),
        "{other:?}"
    );
    assert_eq!(row(&svc, m, remote[1].id).person_id, Some(other.person));
    assert_eq!(print_count(&svc, other.person), None);
    // Too little speech for a new voiceprint: linked, nothing stored.
    let short = save_voice_with(
        &svc,
        m,
        remote[1].id,
        &PersonChoice::New {
            name: "Reese".into(),
        },
        &calibrated(false),
    )
    .unwrap();
    assert!(
        matches!(short.remembered, Remembered::NotEnoughSpeech { .. }),
        "{short:?}"
    );
    assert_eq!(print_count(&svc, short.person), None);
}

#[test]
fn edit_save_voice_never_enrols_the_user_and_writes_nothing_when_refused() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 540);
    let me = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();
    let any = calibrated(true);
    let persons_before = list_persons(&svc).unwrap();
    // "Me" links only to the user, and a new person is not the user: refused, no person made.
    assert_eq!(
        refused(save_voice_with(
            &svc,
            m,
            me.id,
            &PersonChoice::New { name: "Sky".into() },
            &any
        )),
        EditError::MeIsTheUser(me.id)
    );
    assert_eq!(list_persons(&svc).unwrap(), persons_before);
    // Saving "Me" to the user is refused by the enrolment rule, and the link rolls back.
    let user = me.person_id.expect("the Me row is the user's");
    assert_eq!(
        refused(save_voice_with(
            &svc,
            m,
            me.id,
            &PersonChoice::Existing(user),
            &any
        )),
        EditError::MeVoice(me.id)
    );
    assert_eq!(print_count(&svc, user), None);
    // The same with a link that really writes (the Me row named apart from the user's
    // person, so the link rewrites it): the enrolment's refusal rolls the link back.
    assert!(rename_speaker(&svc, m, me.id, "Me in the office").unwrap());
    let renamed = row(&svc, m, me.id);
    assert_eq!(
        refused(save_voice_with(
            &svc,
            m,
            me.id,
            &PersonChoice::Existing(user),
            &any
        )),
        EditError::MeVoice(me.id)
    );
    assert_eq!(row(&svc, m, me.id), renamed, "the link was rolled back");
    assert_eq!(print_count(&svc, user), None);
    // Another voice saved as the user: refused.
    let s = remote_speakers(&svc, m)[0].clone();
    assert_eq!(
        refused(save_voice_with(
            &svc,
            m,
            s.id,
            &PersonChoice::Existing(user),
            &any
        )),
        EditError::NotTheUser(s.id)
    );
    assert_eq!(row(&svc, m, s.id), s);
    assert_eq!(
        refused(save_voice_with(
            &svc,
            m,
            s.id,
            &PersonChoice::New { name: " ".into() },
            &any
        )),
        EditError::InvalidName(NameProblem::Empty)
    );
}

/// The first free "Speaker N" among the other live speakers of `meeting`.
fn free_label(svc: &Services, meeting: EntityId, speaker: EntityId) -> String {
    let taken: Vec<String> = view(svc, meeting)
        .speakers
        .into_iter()
        .filter(|s| s.id != speaker)
        .map(|s| s.display_name)
        .collect();
    first_free_speaker_name(&taken)
}

#[test]
fn edit_not_this_person_unlinks_relabels_and_ignores_a_named_voice() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 550);
    let s = remote_speakers(&svc, m)[0].clone();
    let rowan = link_speaker(
        &svc,
        m,
        s.id,
        &PersonChoice::New {
            name: "Rowan".into(),
        },
    )
    .unwrap()
    .person;
    let linked = row(&svc, m, s.id);
    assert!(can_not_this_person(&**svc.store(), &linked).unwrap());
    let label = free_label(&svc, m, s.id);

    assert_eq!(not_this_person(&svc, m, s.id).unwrap().person, rowan);
    let after = row(&svc, m, s.id);
    assert_eq!(after.person_id, None);
    assert_eq!(after.display_name, label);
    assert!(after.display_name.starts_with("Speaker "));
    assert!(ignored(&svc, m, s.id));
    // T12.93: the ignore is the row's own flag; the entry reads as the pass's naming of the
    // label (its own flag never written), an edit only by the row's flag.
    assert!(after.ignored);
    let e = entry(&svc, m, s.id);
    assert!(!e.ignored && e.edited(&after, legacy_on(&svc)));
    assert_eq!(e.given_person, None);
    // The person stays: only this meeting's row is unlinked.
    assert!(
        list_persons(&svc).unwrap().iter().any(|p| p.id == rowan),
        "Rowan is kept"
    );
    // Not linked any more: nothing to say "not" to.
    assert_eq!(
        refused(not_this_person(&svc, m, s.id)),
        EditError::NoPerson(s.id)
    );
    assert!(!can_not_this_person(&**svc.store(), &after).unwrap());
}

#[test]
fn edit_not_this_person_on_a_recognised_voice_is_carried_by_a_retranscribe() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    // As `edit_ignored_voice_stays_unlinked_when_a_voiceprint_would_name_it`: a meeting
    // processed before the voiceprint exists, re-run after it, names the voice by it.
    let m = processed(&svc, 561);
    let elsewhere = processed(&svc, 560);
    let avery = save_voice_with(
        &svc,
        elsewhere,
        remote_speakers(&svc, elsewhere)[0].id,
        &PersonChoice::New {
            name: "Avery".into(),
        },
        &calibrated(true),
    )
    .unwrap()
    .person;
    let cancel = CancelToken::new();
    retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let voice = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.person_id == Some(avery))
        .expect("the voiceprint names the voice on a re-run");
    let count = print_count(&svc, avery);
    let label = free_label(&svc, m, voice.id);

    assert_eq!(not_this_person(&svc, m, voice.id).unwrap().person, avery);
    // Only recognised here, never saved from this meeting: the voiceprint is untouched.
    assert_eq!(print_count(&svc, avery), count);
    assert!(!entry(&svc, m, voice.id).enrolled);

    // A re-run continues the choice: the same row, its label, unlinked and ignored, and no
    // other row takes the voiceprint's name.
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let HostPost::Done(outcome) = again.post else {
        panic!("{:?}", again.post);
    };
    let carried = outcome.speakers.iter().find(|p| p.id == voice.id).unwrap();
    assert!(carried.ignored && carried.person.is_none(), "{carried:?}");
    let after = row(&svc, m, voice.id);
    assert_eq!(after.person_id, None);
    assert_eq!(after.display_name, label);
    assert!(ignored(&svc, m, voice.id));
    assert!(
        view(&svc, m)
            .speakers
            .iter()
            .all(|s| s.person_id != Some(avery))
    );
    // "Stop ignoring" works as on any ignored voice.
    assert!(unignore_speaker(&svc, m, voice.id).unwrap());
    assert!(!entry(&svc, m, voice.id).ignored && !row(&svc, m, voice.id).ignored);
}

#[test]
fn edit_not_this_person_takes_back_a_voice_saved_from_this_meeting() {
    // T12.77b6: a save records a sample, so "Not" takes exactly it back out.
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 570);
    let s = remote_speakers(&svc, m)[0].id;
    let saved = save_voice_with(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Morgan".into(),
        },
        &calibrated(true),
    )
    .unwrap();
    assert_eq!(print_count(&svc, saved.person), Some(1));
    let done = not_this_person(&svc, m, s).unwrap();
    assert_eq!(done.took_back.taken_back, 1);
    assert!(!done.saved_before_samples);
    assert_eq!(print_count(&svc, saved.person), Some(0));
    assert!(svc.store().samples_from(m, s).unwrap().is_empty());
    assert_eq!(row(&svc, m, s).person_id, None);
}

// T12.93 changed this test: a named voice the final pass kept nothing of is now accepted (the
// ignore lives on its row), so the old `NoStoredVoice` refusal became an acceptance.
#[test]
fn edit_not_this_person_refuses_the_user_and_an_unlinked_voice_and_takes_nothing_stored() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 580);
    let me = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();
    assert_eq!(
        refused(not_this_person(&svc, m, me.id)),
        EditError::UserRow(me.id)
    );
    assert_eq!(row(&svc, m, me.id), me, "nothing written");
    assert!(!can_not_this_person(&**svc.store(), &me).unwrap());
    let s = remote_speakers(&svc, m)[0].clone();
    assert_eq!(
        refused(not_this_person(&svc, m, s.id)),
        EditError::NoPerson(s.id)
    );
    // A named voice the final pass kept nothing of: offered and accepted, nothing taken back.
    let quiet = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Speaker 9",
        "hello there",
        9_000,
        581,
    );
    link_speaker(
        &svc,
        m,
        quiet,
        &PersonChoice::New {
            name: "Sasha".into(),
        },
    )
    .unwrap();
    let linked = row(&svc, m, quiet);
    assert!(can_not_this_person(&**svc.store(), &linked).unwrap());
    let done = not_this_person(&svc, m, quiet).unwrap();
    assert_eq!(done.took_back.taken_back, 0);
    let after = row(&svc, m, quiet);
    assert!(after.person_id.is_none() && after.ignored && ignored(&svc, m, quiet));
    let nobody = EntityId::from_bytes([6; 16]);
    assert_eq!(
        refused(not_this_person(&svc, m, nobody)),
        EditError::SpeakerNotFound(nobody)
    );
}

#[test]
fn edit_not_this_person_waits_while_the_meeting_records_or_finishes() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 590);
    let s = remote_speakers(&svc, m)[0].id;
    link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Jules".into(),
        },
    )
    .unwrap();
    let before = row(&svc, m, s);
    for job in [
        MeetingJob::Processing,
        MeetingJob::Recording,
        MeetingJob::Waiting,
    ] {
        let lease = svc.lease_meeting_for(m, job).unwrap();
        let r = not_this_person(&svc, m, s);
        assert!(
            matches!(r, Err(RuntimeError::MeetingBusy(x)) if x == m),
            "{job:?}: {r:?}"
        );
        assert_eq!(row(&svc, m, s), before, "{job:?}: nothing written");
        drop(lease);
    }
    not_this_person(&svc, m, s).unwrap();
    // The edit's own claim is gone afterwards.
    drop(svc.lease_meeting_alone(m, MeetingJob::Processing).unwrap());
}

#[test]
fn edit_not_this_person_refuses_a_row_linked_to_a_person_merged_into_the_user() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 595);
    let s = remote_speakers(&svc, m)[0].id;
    crate::my_voice::set_my_name(&svc, "Casey Lane").unwrap();
    // Before marking existed, the user named a voice after themselves, then merged that
    // person into themselves: the row now stands for the user.
    let twin = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Casey Lane".into(),
        },
    )
    .unwrap()
    .person;
    let before = row(&svc, m, s);
    assert!(can_not_this_person(&**svc.store(), &before).unwrap());
    let user = merge_into_me(&svc, twin).unwrap();
    assert_eq!(resolved_person(&**svc.store(), twin).unwrap(), Some(user));
    let before = row(&svc, m, s);
    let entry_before = entry(&svc, m, s);

    assert!(!can_not_this_person(&**svc.store(), &before).unwrap());
    assert_eq!(refused(not_this_person(&svc, m, s)), EditError::UserRow(s));
    assert_eq!(row(&svc, m, s), before, "nothing written");
    assert_eq!(entry(&svc, m, s), entry_before, "nothing written");
}

/// Opens `dir`'s store again as an earlier build left it (no `identity-sweep`), so the open
/// sweeps (T14.49c, docs/sync-spec.md 7A.2 P5).
fn reopened_for_the_sweep(dir: &TempDir, svc: Services) -> Services {
    let key = earshot_store::identities::SWEEP_KEY;
    assert!(svc.store().local_value(key).unwrap().is_some());
    svc.store().delete_local_value(key).unwrap();
    drop(svc);
    let (svc, _) = services(dir);
    assert!(svc.store().local_value(key).unwrap().is_some(), "swept");
    svc
}

/// T14.49c (docs/sync-spec.md 7A.11 "Save after the sweep"): an older meeting's unnamed voice
/// keeps its entry through the open's sweep, so naming and saving it enrols the voice (not
/// `NoStoredVoice`) and a re-transcription keeps the name. Positive control: the same with the
/// entry removed by hand gives `NoStoredVoice` and the name is not carried.
#[test]
fn edit_save_after_the_identity_sweep_enrols_and_a_retranscribe_keeps_the_name() {
    for by_hand in [false, true] {
        let dir = TempDir::new("edit-sweep");
        let (svc, _) = services(&dir);
        let m = processed(&svc, 570 + u64::from(by_hand));
        let s = remote_speakers(&svc, m)[0].clone();
        // Beside it, an entry of a row that is gone: the sweep's drop shows that it ran.
        let mut kept = identities_in(&svc.store(), m).unwrap();
        let mut gone = kept[0].clone();
        gone.speaker =
            EntityId::from_bytes([1, 0x93, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 9, 9]);
        if by_hand {
            kept.retain(|i| i.speaker != s.id);
        }
        kept.push(gone.clone());
        crate::local::set(
            &mut svc.store(),
            &record_key(m),
            &crate::identities::encode(&crate::identities::IdentityRecord { speakers: kept }),
        )
        .unwrap();
        let svc = reopened_for_the_sweep(&dir, svc);
        let after = identities_in(&svc.store(), m).unwrap();
        assert!(
            !after.iter().any(|i| i.speaker == gone.speaker),
            "the gone row's entry"
        );
        assert_eq!(after.iter().any(|i| i.speaker == s.id), !by_hand);

        let saved = save_voice_with(
            &svc,
            m,
            s.id,
            &PersonChoice::New {
                name: "Morgan Invented".into(),
            },
            &calibrated(true),
        )
        .unwrap();
        assert_eq!(row(&svc, m, s.id).person_id, Some(saved.person));
        if by_hand {
            assert_eq!(saved.remembered, Remembered::NoStoredVoice, "{saved:?}");
        } else {
            assert!(
                matches!(saved.remembered, Remembered::Enrolled { .. }),
                "{saved:?}"
            );
        }
        let cancel = CancelToken::new();
        retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
        let named = view(&svc, m)
            .speakers
            .into_iter()
            .any(|r| r.display_name == "Morgan Invented" && r.person_id == Some(saved.person));
        assert_eq!(named, !by_hand, "by hand: {by_hand}");
    }
}
