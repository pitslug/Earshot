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
    let identities = identities_in(&svc.store(), meeting).unwrap();
    is_ignored(&row(svc, meeting, id), &identities)
}

#[test]
fn edit_ignore_speaker_keeps_the_label_and_is_carried_by_a_retranscribe() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 500);
    let remote = remote_speakers(&svc, m);
    let s = remote[0].clone();
    let identities = identities_in(&svc.store(), m).unwrap();
    assert!(can_ignore(&s, &identities));
    assert!(!is_ignored(&s, &identities));

    assert!(ignore_speaker(&svc, m, s.id).unwrap());
    assert!(
        !ignore_speaker(&svc, m, s.id).unwrap(),
        "a repeat writes nothing"
    );
    assert!(ignored(&svc, m, s.id));
    // The row is not written: the label and the pass's stamp stay, and the choice is the
    // entry's, an edit of its own.
    let after = row(&svc, m, s.id);
    assert_eq!(after.display_name, s.display_name);
    assert_eq!(after.meta.updated_at, s.meta.updated_at);
    assert_eq!(after.person_id, None);
    let e = entry(&svc, m, s.id);
    assert!(e.ignored && e.edited(&after) && !e.user_edited);
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
    assert!(ignored(&svc, m, s.id));

    // Un-ignored: it needs a name again and reads as the pass's naming.
    assert!(unignore_speaker(&svc, m, s.id).unwrap());
    assert!(!unignore_speaker(&svc, m, s.id).unwrap());
    assert!(!ignored(&svc, m, s.id));
    assert!(!entry(&svc, m, s.id).ignored);
}

#[test]
fn edit_ignore_refuses_the_user_a_named_voice_and_a_voice_with_nothing_stored() {
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
    let identities = identities_in(&svc.store(), m).unwrap();
    assert!(!can_ignore(&row(&svc, m, s), &identities));
    assert!(!can_ignore(&me, &identities));
    // A voice the final pass kept nothing of: the choice has nowhere to live.
    let quiet = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Speaker 9",
        "hello there",
        9_000,
        511,
    );
    assert!(!can_ignore(&row(&svc, m, quiet), &identities));
    assert_eq!(
        refused(ignore_speaker(&svc, m, quiet)),
        EditError::NoStoredVoice(quiet)
    );
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
    assert!(!entry(&svc, m, remote[0].id).ignored);
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
    assert!(!entry(&svc, m, remote[1].id).ignored);
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
    assert!(entry(&svc, m, remote[0].id).enrolled);
    // Saved again to the same person: linked already, remembered already.
    let again = save_voice_with(
        &svc,
        m,
        remote[0].id,
        &PersonChoice::Existing(saved.person),
        &any,
    )
    .unwrap();
    assert_eq!(again.remembered, Remembered::AlreadyEnrolled);
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
