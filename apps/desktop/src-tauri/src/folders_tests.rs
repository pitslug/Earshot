//! Folder command tests (T9.5c): the list carries resolved folders, refusals are fixed copy
//! that never quotes a name, deletion counts, and the newer-database error kind.

use std::sync::Arc;

use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{Meeting, MeetingRepo, Stamp};
use earshot_core::prompt::Template;
use earshot_runtime::{MeetingJob, Services};
use earshot_store::DataDir;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::ipc::{AppState, NEWER_DATA_COPY, NEWER_DATA_KIND, meeting_list};
use crate::testutil::{T0, TempDir, newer_database, services};

/// A finished meeting row, stamped as the runtime stamps one, filed under the raw
/// `folder_id` given (which may name a folder that does not exist). Distinct seeds per call.
fn meeting(
    svc: &Services,
    title: &str,
    started: u64,
    folder: Option<EntityId>,
    seed: u64,
) -> String {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new(title, started, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    m.folder_id = folder;
    let id = m.id;
    store.put_meeting(m).unwrap();
    id.to_string()
}

fn id(text: &str) -> EntityId {
    text.parse().unwrap()
}

/// (title, folder id) of every listed meeting, newest first.
fn filed(svc: &Services) -> Vec<(String, Option<String>)> {
    meeting_list(svc)
        .unwrap()
        .into_iter()
        .map(|m| (m.title, m.folder_id))
        .collect()
}

#[test]
fn folders_are_listed_by_name_ignoring_case() {
    let dir = TempDir::new("folders-list");
    let svc = services(&dir);
    assert_eq!(folder_list(&svc).unwrap(), []);
    let walnut = create(&svc, "walnut grove").unwrap();
    let apple = create(&svc, "  Apple orchard ").unwrap();
    let names: Vec<(String, String)> = folder_list(&svc)
        .unwrap()
        .into_iter()
        .map(|f| (f.id, f.name))
        .collect();
    assert_eq!(
        names,
        [
            (apple, "Apple orchard".to_owned()),
            (walnut.clone(), "walnut grove".to_owned())
        ]
    );
    rename(&svc, &walnut, "Almond row").unwrap();
    assert_eq!(folder_list(&svc).unwrap()[0].name, "Almond row");
}

#[test]
fn folders_meeting_list_carries_the_resolved_folder_never_the_raw_one() {
    let dir = TempDir::new("folders-resolved");
    let svc = services(&dir);
    let orchard = create(&svc, "Orchard").unwrap();
    meeting(&svc, "Filed", T0 + 3_000, Some(id(&orchard)), 1);
    // A folder this device has never seen (sync may deliver the meeting first).
    meeting(
        &svc,
        "Dangling",
        T0 + 2_000,
        Some(EntityId::from_bytes([9; 16])),
        2,
    );
    meeting(&svc, "Loose", T0 + 1_000, None, 3);
    assert_eq!(
        filed(&svc),
        [
            ("Filed".to_owned(), Some(orchard)),
            ("Dangling".to_owned(), None),
            ("Loose".to_owned(), None),
        ]
    );
}

#[test]
fn folders_move_files_and_unfiles_a_meeting() {
    let dir = TempDir::new("folders-move");
    let svc = services(&dir);
    let orchard = create(&svc, "Orchard").unwrap();
    let m = meeting(&svc, "Pruning plan", T0, None, 4);
    move_meeting(&svc, &m, Some(&orchard)).unwrap();
    assert_eq!(filed(&svc)[0].1.as_deref(), Some(orchard.as_str()));
    move_meeting(&svc, &m, None).unwrap();
    assert_eq!(filed(&svc)[0].1, None);
}

#[test]
fn folders_refusals_are_fixed_copy_that_never_quotes_the_name() {
    let dir = TempDir::new("folders-refusals");
    let svc = services(&dir);
    let orchard = create(&svc, "Quincefield").unwrap();

    let taken = create(&svc, "QUINCEFIELD").unwrap_err();
    assert_eq!(taken.kind, "folder_name_taken");
    assert_eq!(taken.message, "Another folder already has this name.");
    let other = create(&svc, "Medlar").unwrap();
    let taken = rename(&svc, &other, "quincefield").unwrap_err();
    assert_eq!(taken.kind, "folder_name_taken");

    let empty = create(&svc, "   ").unwrap_err();
    assert_eq!(
        (empty.kind.as_str(), empty.message.as_str()),
        ("invalid_name", "Enter a name.")
    );
    let tab = rename(&svc, &orchard, "Quince\tfield").unwrap_err();
    assert_eq!(tab.kind, "invalid_name");

    let bad = rename(&svc, "not-an-id", "Pears").unwrap_err();
    assert_eq!(bad.kind, "invalid_id");
    let gone = EntityId::from_bytes([3; 16]).to_string();
    let missing = rename(&svc, &gone, "Pears").unwrap_err();
    assert_eq!(missing.kind, "folder_not_found");
    let m = meeting(&svc, "Pruning plan", T0, None, 5);
    let missing = move_meeting(&svc, &m, Some(&gone)).unwrap_err();
    assert_eq!(missing.kind, "folder_not_found");

    for e in [taken, empty, tab, missing] {
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.to_lowercase().contains("quince"), "{json}");
    }
}

#[test]
fn folders_a_recording_meeting_cannot_be_moved() {
    let dir = TempDir::new("folders-recording");
    let svc = services(&dir);
    let orchard = create(&svc, "Orchard").unwrap();
    let m = meeting(&svc, "Live", T0, None, 6);
    let lease = svc
        .lease_meeting_for(id(&m), MeetingJob::Recording)
        .unwrap();
    let e = move_meeting(&svc, &m, Some(&orchard)).unwrap_err();
    assert_eq!(e.kind, "meeting_recording");
    assert_eq!(
        e.message,
        "A meeting can be moved to a folder once it has finished recording."
    );
    drop(lease);
    move_meeting(&svc, &m, Some(&orchard)).unwrap();
}

#[test]
fn folders_delete_keeping_meetings_unfiles_them() {
    let dir = TempDir::new("folders-keep");
    let svc = services(&dir);
    let orchard = create(&svc, "Orchard").unwrap();
    meeting(&svc, "One", T0 + 1_000, Some(id(&orchard)), 7);
    meeting(&svc, "Two", T0, Some(id(&orchard)), 8);
    let done = delete(&svc, &orchard, false).unwrap();
    assert_eq!(
        done,
        FolderDeleted {
            folder_deleted: true,
            meetings_deleted: 0,
            meetings_left: 0,
            meetings_unfiled: 2,
        }
    );
    assert_eq!(folder_list(&svc).unwrap(), []);
    assert_eq!(
        filed(&svc),
        [("One".to_owned(), None), ("Two".to_owned(), None)]
    );
}

#[test]
fn folders_delete_with_meetings_counts_busy_ones_and_keeps_the_folder() {
    let dir = TempDir::new("folders-with");
    let svc = services(&dir);
    let orchard = create(&svc, "Orchard").unwrap();
    let busy = meeting(&svc, "Busy", T0 + 2_000, Some(id(&orchard)), 9);
    meeting(&svc, "Idle", T0 + 1_000, Some(id(&orchard)), 10);
    meeting(&svc, "Elsewhere", T0, None, 11);
    let lease = svc
        .lease_meeting_for(id(&busy), MeetingJob::Processing)
        .unwrap();

    let done = delete(&svc, &orchard, true).unwrap();
    assert_eq!(
        done,
        FolderDeleted {
            folder_deleted: false,
            meetings_deleted: 1,
            meetings_left: 1,
            meetings_unfiled: 0,
        }
    );
    assert_eq!(folder_list(&svc).unwrap().len(), 1);
    assert_eq!(
        filed(&svc),
        [
            ("Busy".to_owned(), Some(orchard.clone())),
            ("Elsewhere".to_owned(), None)
        ]
    );

    drop(lease);
    let done = delete(&svc, &orchard, true).unwrap();
    assert!(done.folder_deleted);
    assert_eq!((done.meetings_deleted, done.meetings_left), (1, 0));
    assert_eq!(filed(&svc), [("Elsewhere".to_owned(), None)]);
    assert_eq!(folder_list(&svc).unwrap(), []);
}

#[test]
fn folders_newer_database_is_its_own_error_kind_with_fixed_copy() {
    let dir = TempDir::new("folders-newer");
    let state = AppState::open(newer_database(&dir));
    let Err(e) = state.services() else {
        panic!("a newer database must be refused");
    };
    assert_eq!(e.kind, NEWER_DATA_KIND);
    assert_eq!(e.message, NEWER_DATA_COPY);
    assert!(!e.retryable);
    // A database of this version opens.
    let fresh = AppState::open(DataDir::new(dir.0.join("fresh")));
    assert!(fresh.services().is_ok());
}
