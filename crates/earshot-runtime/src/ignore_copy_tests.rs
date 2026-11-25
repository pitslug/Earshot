//! The one-off copy of legacy ignores onto the speaker rows (T12.93). Every name contains
//! `ignore`; every name is invented.

use std::sync::Arc;

use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::EntityId;
use earshot_core::model::{MeetingRepo, Speaker, Stamp};
use earshot_core::voiceprint::{Person, VoiceprintRepo};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::identities::{IdentityRecord, SpeakerIdentity, encode};
use crate::testutil::{TempDir, meeting, services};

/// A new speaker row of `meeting` (and, with `person_name`, a new person it links to).
fn add(
    svc: &Services,
    meeting: EntityId,
    role: SpeakerRole,
    name: &str,
    person_name: Option<&str>,
    seed: u64,
) -> Speaker {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let person = person_name.map(|n| {
        let p = Person::new(n, false, &mut stamp);
        store.put_person(p.clone()).unwrap();
        p.id
    });
    let s = Speaker::new(meeting, role, name, person, &mut stamp);
    store.put_speaker(s.clone()).unwrap();
    s
}

/// A legacy entry for `row`, its ignore flag as given.
fn entry(row: &Speaker, ignored: bool) -> SpeakerIdentity {
    SpeakerIdentity {
        speaker: row.id,
        track: TrackId::Reference,
        centroid: Embedding::new(EmbeddingModelId::new("m@1"), vec![0.6, 0.8]),
        given_name: row.display_name.clone(),
        given_person: row.person_id,
        given_stamp: Some(row.meta.updated_at),
        user_edited: false,
        enrolled: false,
        ignored,
    }
}

fn put_record(svc: &Services, meeting: EntityId, speakers: Vec<SpeakerIdentity>) {
    let key = crate::identities::record_key(meeting);
    crate::local::set(
        &mut svc.store(),
        &key,
        &encode(&IdentityRecord { speakers }),
    )
    .unwrap();
}

fn row(svc: &Services, meeting: EntityId, id: EntityId) -> Speaker {
    svc.store()
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap()
}

#[test]
fn ignore_copy_flags_only_unnamed_live_voices_and_runs_once() {
    let dir = TempDir::new("ignore-copy");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented review", 1);
    let plain = add(&svc, m, SpeakerRole::Remote, "Speaker 2", None, 10);
    // Renamed here and ignored: still unnamed (no person), so the ignore is kept.
    let renamed = add(&svc, m, SpeakerRole::Remote, "Visitor", None, 11);
    let linked = add(
        &svc,
        m,
        SpeakerRole::Remote,
        "Rowan Invented",
        Some("Rowan Invented"),
        12,
    );
    let me = add(&svc, m, SpeakerRole::Me, "Me", None, 13);
    let mut gone = add(&svc, m, SpeakerRole::Remote, "Speaker 4", None, 14);
    let not_ignored = add(&svc, m, SpeakerRole::Remote, "Speaker 5", None, 15);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        gone.meta.mark_deleted(hlc, &*clock);
        store.put_speaker(gone.clone()).unwrap();
    }
    put_record(
        &svc,
        m,
        vec![
            entry(&plain, true),
            entry(&renamed, true),
            entry(&linked, true),
            entry(&me, true),
            entry(&gone, true),
            entry(&not_ignored, false),
        ],
    );
    // A deleted meeting's rows are never touched.
    let deleted = meeting(&svc, "Invented gone", 2);
    let in_deleted = add(&svc, deleted, SpeakerRole::Remote, "Speaker 7", None, 16);
    put_record(&svc, deleted, vec![entry(&in_deleted, true)]);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut mt = store.meeting(deleted).unwrap().unwrap();
        mt.meta.mark_deleted(hlc, &*clock);
        store.put_meeting(mt).unwrap();
    }

    // Before the copy the entry flag is a legacy ignore, honoured by the one rule.
    let legacy = crate::edit::legacy_ignores(&svc.store(), m).unwrap();
    assert!(legacy.contains(&plain.id) && !legacy.contains(&not_ignored.id));
    assert!(crate::edit::is_ignored(&plain, &legacy));
    assert!(!crate::edit::is_ignored(&linked, &legacy), "a linked voice");
    assert!(!crate::edit::is_ignored(&me, &legacy), "the user");
    assert!(!copied(&svc.store()).unwrap());

    assert_eq!(copy_once(&svc).unwrap(), 2);
    assert!(copied(&svc.store()).unwrap());
    for (s, want) in [
        (&plain, true),
        (&renamed, true),
        (&linked, false),
        (&me, false),
        (&gone, false),
        (&not_ignored, false),
    ] {
        let now = row(&svc, m, s.id);
        assert_eq!(now.ignored, want, "{}", s.display_name);
        // Only the flag changes: names and links stay as they were.
        assert_eq!(
            (&now.display_name, now.person_id, now.role),
            (&s.display_name, s.person_id, s.role)
        );
        assert_eq!(
            now.meta.updated_at != s.meta.updated_at,
            want,
            "re-stamped only when flagged"
        );
    }
    assert!(!row(&svc, deleted, in_deleted.id).ignored);
    // After the copy the entry flag is never read: only the row counts.
    let legacy = crate::edit::legacy_ignores(&svc.store(), m).unwrap();
    assert!(legacy.is_empty());
    assert!(crate::edit::is_ignored(&row(&svc, m, plain.id), &legacy));

    // Once: a second run writes nothing, even over a new legacy flag.
    put_record(&svc, m, vec![entry(&not_ignored, true)]);
    assert_eq!(copy_once(&svc).unwrap(), 0);
    assert!(!row(&svc, m, not_ignored.id).ignored);
}

#[test]
fn ignore_copy_record_key_is_pinned() {
    assert_eq!(DeviceRecord::IgnoreCopied.key().as_str(), "ignore-copied");
    assert_eq!(COPIED_KEY, "ignore-copied");
}

#[test]
fn ignore_copy_legacy_of_reads_nothing_after_the_copy() {
    let dir = TempDir::new("ignore-legacy");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented sync", 3);
    let s = add(&svc, m, SpeakerRole::Remote, "Speaker 2", None, 20);
    let entries = vec![entry(&s, true)];
    assert_eq!(legacy_of(false, &entries).len(), 1);
    assert!(legacy_of(true, &entries).is_empty());
}
