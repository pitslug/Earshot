//! Folder tests (T9.5b, docs/folders-design.md "Tests to write"). Every name contains
//! `folders`; every name here is invented.

use std::sync::Arc;

use earshot_core::identity::EntityId;
use earshot_core::model::{FolderRepo, MeetingRepo, delete_folder};
use earshot_core::summary::SummaryV1;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::MeetingJob;
use crate::testutil::{TempDir, meeting, services};
use crate::view::MeetingView;

fn refused(r: Result<impl std::fmt::Debug, RuntimeError>) -> EditError {
    match r {
        Err(RuntimeError::Edit(e)) => e,
        other => panic!("expected an edit refusal, got {other:?}"),
    }
}

fn stored_folder_id(svc: &Services, meeting: EntityId) -> Option<EntityId> {
    svc.store().meeting(meeting).unwrap().unwrap().folder_id
}

fn names(svc: &Services) -> Vec<String> {
    list(svc).unwrap().into_iter().map(|f| f.name).collect()
}

/// Writes `folder_id` straight into the meeting row, as sync could deliver it.
fn set_raw_folder(svc: &Services, meeting: EntityId, folder: Option<EntityId>) {
    let mut db = svc.store();
    let mut m = db.meeting(meeting).unwrap().unwrap();
    m.folder_id = folder;
    db.put_meeting(m).unwrap();
}

#[test]
fn folders_are_listed_by_name_with_case_folded() {
    let dir = TempDir::new("folders-list");
    let (svc, _clock) = services(&dir);
    assert!(list(&svc).unwrap().is_empty());
    for name in ["Charlie", "beta", "  Alpha  "] {
        create(&svc, name).unwrap();
    }
    assert_eq!(
        names(&svc),
        ["Alpha", "beta", "Charlie"],
        "trimmed and folded"
    );
}

#[test]
fn folders_names_follow_the_name_rule() {
    let dir = TempDir::new("folders-name-rule");
    let (svc, _clock) = services(&dir);
    assert_eq!(
        refused(create(&svc, "   ")),
        EditError::InvalidName(crate::edit::NameProblem::Empty)
    );
    assert_eq!(
        refused(create(&svc, "Two\nlines")),
        EditError::InvalidName(crate::edit::NameProblem::ControlCharacter)
    );
    assert_eq!(
        refused(create(&svc, &"x".repeat(crate::edit::MAX_NAME_CHARS + 1))),
        EditError::InvalidName(crate::edit::NameProblem::TooLong)
    );
    let id = create(&svc, "Garden club").unwrap();
    assert_eq!(
        refused(rename(&svc, id, "\t")),
        EditError::InvalidName(crate::edit::NameProblem::Empty)
    );
    assert!(
        list(&svc).unwrap().len() == 1,
        "nothing written by a refusal"
    );
}

#[test]
fn folders_duplicate_names_are_refused_ignoring_case_without_quoting_the_name() {
    let dir = TempDir::new("folders-duplicate");
    let (svc, _clock) = services(&dir);
    let clients = create(&svc, "Lantern Works").unwrap();
    let err = refused(create(&svc, "lantern works"));
    assert_eq!(err, EditError::FolderNameTaken(clients));
    let text = RuntimeError::Edit(err).to_string().to_lowercase();
    assert!(!text.contains("lantern"), "{text}");

    // A non-ASCII case pair.
    let accent = create(&svc, "\u{c9}quipe Nord").unwrap();
    assert_eq!(
        refused(create(&svc, "\u{e9}QUIPE nord")),
        EditError::FolderNameTaken(accent)
    );

    // Renaming onto another live folder's name is refused too.
    let other = create(&svc, "Orchard").unwrap();
    assert_eq!(
        refused(rename(&svc, other, "LANTERN WORKS")),
        EditError::FolderNameTaken(clients)
    );
    // Folded per character and compared by code point, so an accented letter sorts after z.
    assert_eq!(
        names(&svc),
        ["Lantern Works", "Orchard", "\u{c9}quipe Nord"]
    );
}

#[test]
fn folders_rename_to_its_own_name_in_another_case_is_allowed() {
    let dir = TempDir::new("folders-rename-case");
    let (svc, _clock) = services(&dir);
    let id = create(&svc, "harbour visits").unwrap();
    assert!(rename(&svc, id, "Harbour Visits").unwrap());
    assert_eq!(names(&svc), ["Harbour Visits"]);
    assert!(!rename(&svc, id, " Harbour Visits ").unwrap(), "same name");
    assert!(rename(&svc, id, "Quay").unwrap());
    assert_eq!(names(&svc), ["Quay"]);
    let gone = EntityId::from_bytes([7; 16]);
    assert_eq!(
        refused(rename(&svc, gone, "Anything")),
        EditError::FolderNotFound(gone)
    );
}

#[test]
fn folders_a_dangling_or_tombstoned_folder_id_resolves_to_no_folder() {
    let dir = TempDir::new("folders-dangling");
    let (svc, _clock) = services(&dir);
    let unknown = meeting(&svc, "Unknown folder", 1);
    let tombstoned = meeting(&svc, "Tombstoned folder", 2);
    let filed = meeting(&svc, "Filed", 3);
    let live = create(&svc, "Live").unwrap();
    let dead = create(&svc, "Dead").unwrap();
    set_raw_folder(&svc, unknown, Some(EntityId::from_bytes([9; 16])));
    move_meeting(&svc, tombstoned, Some(dead)).unwrap();
    move_meeting(&svc, filed, Some(live)).unwrap();
    // Only the folder row goes (as a sync from another device could deliver it): the
    // meeting still names it.
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        assert!(delete_folder(store, dead, hlc, &*clock).unwrap());
    }
    assert_eq!(stored_folder_id(&svc, tombstoned), Some(dead));

    assert_eq!(folder_of(&svc, unknown).unwrap(), None);
    assert_eq!(folder_of(&svc, tombstoned).unwrap(), None);
    assert_eq!(
        folder_of(&svc, filed).unwrap(),
        Some(FolderItem {
            id: live,
            name: "Live".into()
        })
    );
    assert_eq!(
        meeting_folders(&svc).unwrap(),
        BTreeMap::from([(filed, live)]),
        "only resolved folders are listed"
    );
    assert_eq!(names(&svc), ["Live"]);
    // A tombstoned folder is no target.
    let other = meeting(&svc, "Other", 4);
    assert_eq!(
        refused(move_meeting(&svc, other, Some(dead))),
        EditError::FolderNotFound(dead)
    );
    assert_eq!(stored_folder_id(&svc, other), None);
}

#[test]
fn folders_move_is_refused_while_the_meeting_records() {
    let dir = TempDir::new("folders-recording");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Recording now", 1);
    let f = create(&svc, "Calls").unwrap();
    let recording = svc.lease_meeting_for(m, MeetingJob::Recording).unwrap();
    assert_eq!(
        refused(move_meeting(&svc, m, Some(f))),
        EditError::MeetingRecording(m)
    );
    assert_eq!(stored_folder_id(&svc, m), None);
    drop(recording);

    // Finishing (a final pass) or a job does not stop it: the folder is the user's field.
    let processing = svc.lease_meeting_for(m, MeetingJob::Processing).unwrap();
    let job = svc.lease_meeting(m).unwrap();
    assert!(move_meeting(&svc, m, Some(f)).unwrap());
    assert!(!move_meeting(&svc, m, Some(f)).unwrap(), "already there");
    drop((processing, job));
    assert_eq!(stored_folder_id(&svc, m), Some(f));
    assert!(move_meeting(&svc, m, None).unwrap());
    assert_eq!(stored_folder_id(&svc, m), None);

    let gone = EntityId::from_bytes([5; 16]);
    assert_eq!(
        refused(move_meeting(&svc, gone, Some(f))),
        EditError::MeetingNotFound(gone)
    );
}

#[test]
fn folders_move_during_store_summary_keeps_the_folder() {
    let dir = TempDir::new("folders-summary");
    let (svc, _clock) = services(&dir);
    // No title, so the summary writes the meeting row (it fills an empty title).
    let m = meeting(&svc, "", 1);
    let f = create(&svc, "Planning").unwrap();
    // The summary read the meeting before the move.
    let view = MeetingView::load(&**svc.store(), m).unwrap().unwrap();
    assert_eq!(view.meeting.folder_id, None);
    move_meeting(&svc, m, Some(f)).unwrap();
    let summary = SummaryV1 {
        title: "Invented summary title".into(),
        tldr: "Nothing real.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    };
    let (_, title_set) = crate::summary::store_summary(
        &svc,
        &view,
        "test-model".into(),
        &summary,
        &mut ChaCha8Rng::seed_from_u64(2),
    )
    .unwrap();
    assert!(title_set, "the summary did write the meeting row");
    let row = svc.store().meeting(m).unwrap().unwrap();
    assert_eq!(row.title, "Invented summary title");
    assert_eq!(row.folder_id, Some(f), "the move survived the summary");
}

#[test]
fn folders_delete_keeping_meetings_clears_its_members_only() {
    let dir = TempDir::new("folders-delete-keep");
    let (svc, _clock) = services(&dir);
    let a = meeting(&svc, "First", 1);
    let b = meeting(&svc, "Second", 2);
    let elsewhere = meeting(&svc, "Elsewhere", 3);
    let f = create(&svc, "Workshop").unwrap();
    let g = create(&svc, "Kitchen").unwrap();
    move_meeting(&svc, a, Some(f)).unwrap();
    move_meeting(&svc, b, Some(f)).unwrap();
    move_meeting(&svc, elsewhere, Some(g)).unwrap();

    let done = delete_keeping_meetings(&svc, f).unwrap();
    assert_eq!(
        done,
        FolderDeletion {
            folder_deleted: true,
            meetings_unfiled: 2,
            ..done.clone()
        }
    );
    assert_eq!((done.meetings_deleted, done.meetings_left), (0, 0));
    assert_eq!(stored_folder_id(&svc, a), None);
    assert_eq!(stored_folder_id(&svc, b), None);
    assert_eq!(stored_folder_id(&svc, elsewhere), Some(g));
    for m in [a, b] {
        let row = svc.store().meeting(m).unwrap().unwrap();
        assert!(!row.meta.deleted, "the meetings stay");
        assert_eq!(
            MeetingView::load(&**svc.store(), m)
                .unwrap()
                .unwrap()
                .segments
                .len(),
            2
        );
    }
    let tomb = svc.store().folder(f).unwrap().unwrap();
    assert!(tomb.meta.deleted && tomb.name.is_empty(), "{tomb:?}");
    assert_eq!(names(&svc), ["Kitchen"]);
    assert_eq!(
        refused(delete_keeping_meetings(&svc, f)),
        EditError::FolderNotFound(f)
    );
}

#[test]
fn folders_two_deletions_in_a_row_do_not_collide() {
    let dir = TempDir::new("folders-two-deletions");
    let (svc, _clock) = services(&dir);
    let one = create(&svc, "One").unwrap();
    let two = create(&svc, "Two").unwrap();
    assert!(delete_keeping_meetings(&svc, one).unwrap().folder_deleted);
    assert!(delete_with_meetings(&svc, two).unwrap().folder_deleted);
    assert!(list(&svc).unwrap().is_empty());
    // Tombstones hold one cleared name and never count against a new name.
    create(&svc, "One").unwrap();
    create(&svc, "two").unwrap();
    assert_eq!(names(&svc), ["One", "two"]);
}

#[test]
fn folders_delete_with_meetings_deletes_them_and_the_folder() {
    let dir = TempDir::new("folders-delete-with");
    let (svc, _clock) = services(&dir);
    let a = meeting(&svc, "Gone one", 1);
    let b = meeting(&svc, "Gone two", 2);
    let stays = meeting(&svc, "Not filed", 3);
    let f = create(&svc, "Retired").unwrap();
    move_meeting(&svc, a, Some(f)).unwrap();
    move_meeting(&svc, b, Some(f)).unwrap();
    let done = delete_with_meetings(&svc, f).unwrap();
    assert!(done.folder_deleted);
    assert_eq!(
        (
            done.meetings_deleted,
            done.meetings_left,
            done.meetings_unfiled
        ),
        (2, 0, 0)
    );
    for m in [a, b] {
        assert!(svc.store().meeting(m).unwrap().unwrap().meta.deleted);
    }
    assert!(!svc.store().meeting(stays).unwrap().unwrap().meta.deleted);
    assert!(list(&svc).unwrap().is_empty());
}

#[test]
fn folders_delete_with_a_busy_meeting_leaves_the_folder_holding_it() {
    let dir = TempDir::new("folders-delete-busy");
    let (svc, _clock) = services(&dir);
    let free = meeting(&svc, "Free", 1);
    let busy = meeting(&svc, "Busy", 2);
    let f = create(&svc, "Mixed").unwrap();
    move_meeting(&svc, free, Some(f)).unwrap();
    move_meeting(&svc, busy, Some(f)).unwrap();
    // A re-summarise or finishing holds the meeting: its deletion is refused MeetingBusy.
    let lease = svc.lease_meeting_for(busy, MeetingJob::Processing).unwrap();

    let done = delete_with_meetings(&svc, f).unwrap();
    assert_eq!(
        done,
        FolderDeletion {
            folder_deleted: false,
            meetings_deleted: 1,
            meetings_left: 1,
            meetings_unfiled: 0,
            checkpoint_pending: false,
        }
    );
    assert!(svc.store().meeting(free).unwrap().unwrap().meta.deleted);
    assert_eq!(
        stored_folder_id(&svc, busy),
        Some(f),
        "it stays in the folder"
    );
    assert_eq!(folder_of(&svc, busy).unwrap().map(|i| i.id), Some(f));
    assert_eq!(names(&svc), ["Mixed"]);

    // Once free, a second try finishes the job.
    drop(lease);
    let done = delete_with_meetings(&svc, f).unwrap();
    assert!(done.folder_deleted);
    assert_eq!((done.meetings_deleted, done.meetings_left), (1, 0));
    assert!(svc.store().meeting(busy).unwrap().unwrap().meta.deleted);
    assert!(list(&svc).unwrap().is_empty());
}

#[test]
fn folders_a_meeting_moved_in_during_the_delete_loop_ends_unfiled() {
    let dir = TempDir::new("folders-delete-race");
    let (svc, _clock) = services(&dir);
    let filed = meeting(&svc, "Filed", 1);
    let late = meeting(&svc, "Moved in late", 2);
    let f = create(&svc, "Moving").unwrap();
    move_meeting(&svc, filed, Some(f)).unwrap();

    // The hook runs with no lock held: moving here would deadlock otherwise.
    let mut moved = false;
    let done = delete_with_meetings_then(&svc, f, &mut |_| {
        if !moved {
            move_meeting(&svc, late, Some(f)).unwrap();
            moved = true;
        }
    })
    .unwrap();
    assert!(moved);
    assert!(done.folder_deleted);
    assert_eq!(
        (
            done.meetings_deleted,
            done.meetings_left,
            done.meetings_unfiled
        ),
        (1, 0, 1)
    );
    let row = svc.store().meeting(late).unwrap().unwrap();
    assert!(!row.meta.deleted, "a meeting moved in late is not deleted");
    assert_eq!(row.folder_id, None, "and is not left naming a tombstone");
    assert!(svc.store().meeting(filed).unwrap().unwrap().meta.deleted);
}

#[test]
fn folders_delete_of_an_unknown_folder_is_refused() {
    let dir = TempDir::new("folders-delete-unknown");
    let (svc, _clock) = services(&dir);
    let ghost = EntityId::from_bytes([3; 16]);
    assert_eq!(
        refused(delete_with_meetings(&svc, ghost)),
        EditError::FolderNotFound(ghost)
    );
    assert_eq!(
        refused(delete_keeping_meetings(&svc, ghost)),
        EditError::FolderNotFound(ghost)
    );
}
