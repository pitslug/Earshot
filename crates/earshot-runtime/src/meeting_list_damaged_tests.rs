//! T14.64: one meeting row that does not read never fails a reader that lists every meeting.
//! The meeting list, search, the folder map, a rename of a person, the recovery at start and
//! an export of every meeting each leave it out (the list and search count it). Damage is made
//! by SQL from a second connection (a title that is not UTF-8); every name is invented; each
//! test runs with the damage and without it (the control).

use std::collections::BTreeMap;
use std::sync::Arc;

use earshot_core::export::ExportOptions;
use earshot_core::identity::{EntityId, SyncMeta};
use earshot_core::model::MeetingRepo;
use earshot_core::voiceprint::{Person, VoiceprintRepo};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use crate::Services;
use crate::testutil::{TempDir, meeting, services};

struct World {
    svc: Services,
    dir: TempDir,
}

impl World {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let (svc, _) = services(&dir);
        Self { svc, dir }
    }

    /// Makes `meeting`'s row unreadable: its title is no longer UTF-8.
    fn damage(&self, meeting: EntityId) {
        let db = earshot_store::DataDir::new(self.dir.0.join("data")).db_path();
        let c = rusqlite::Connection::open(db).unwrap();
        let changed = c
            .execute(
                "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                rusqlite::params![meeting.as_uuid().as_bytes().to_vec()],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }

    /// Three meetings; the middle one damaged when `damaged`. Returns (first, middle, last).
    fn three(&self, damaged: bool) -> (EntityId, EntityId, EntityId) {
        let first = meeting(&self.svc, "Invented planning", 101);
        let middle = meeting(&self.svc, "Invented review", 102);
        let last = meeting(&self.svc, "Invented retro", 103);
        if damaged {
            self.damage(middle);
        }
        (first, middle, last)
    }
}

/// The meeting list lists the readable meetings and counts the damaged one; the control
/// counts nothing and lists all three.
#[test]
fn meeting_list_damaged_row_is_left_out_of_the_list_and_counted() {
    for damaged in [true, false] {
        let w = World::new("list-damaged");
        let (first, middle, last) = w.three(damaged);
        let read = w.svc.live_meetings(|_, m| Ok(m.id)).unwrap();
        assert_eq!(read.damaged, usize::from(damaged));
        let mut ids: Vec<EntityId> = read.meetings.iter().map(|(_, _, id)| *id).collect();
        ids.sort();
        let mut want = vec![first, last];
        if !damaged {
            want.push(middle);
        }
        want.sort();
        assert_eq!(ids, want);
    }
}

/// Search finds the readable meetings and counts the damaged one as unreadable; the control
/// finds all three with nothing unreadable.
#[test]
fn meeting_list_damaged_row_never_fails_a_search() {
    for damaged in [true, false] {
        let w = World::new("search-damaged");
        let (_, middle, _) = w.three(damaged);
        let found = crate::search::search_meetings(&w.svc, "release").unwrap();
        assert_eq!(found.unreadable, usize::from(damaged));
        assert_eq!(found.hits.len(), if damaged { 2 } else { 3 });
        assert_eq!(
            found.hits.iter().any(|h| h.meeting == middle),
            !damaged,
            "{found:?}"
        );
    }
}

/// The folder map holds the readable meetings' folders; the control holds all three.
#[test]
fn meeting_list_damaged_row_never_fails_the_folders_of_the_list() {
    for damaged in [true, false] {
        let w = World::new("folders-damaged");
        let (first, middle, last) = w.three(false);
        let folder = crate::folders::create(&w.svc, "Invented folder").unwrap();
        for m in [first, middle, last] {
            crate::folders::move_meeting(&w.svc, m, Some(folder)).unwrap();
        }
        if damaged {
            w.damage(middle);
        }
        let map = crate::folders::meeting_folders(&w.svc).unwrap();
        let mut want: BTreeMap<EntityId, EntityId> =
            [(first, folder), (last, folder)].into_iter().collect();
        if !damaged {
            want.insert(middle, folder);
        }
        assert_eq!(map, want);
    }
}

/// A folder row that does not read (a name that is not UTF-8): the folder list leaves it out,
/// the folder map reads a meeting in it as in no folder and counts it, and the meeting's own
/// folder reads as none; the other folder is untouched. Control: nothing counted, the meeting
/// keeps its folder.
#[test]
fn meeting_list_damaged_folder_row_never_fails_the_folders_of_the_list() {
    for damaged in [true, false] {
        let w = World::new("folder-damaged");
        let (first, _, last) = w.three(false);
        let broken = crate::folders::create(&w.svc, "Invented Quillmere").unwrap();
        let kept = crate::folders::create(&w.svc, "Invented archive").unwrap();
        crate::folders::move_meeting(&w.svc, first, Some(broken)).unwrap();
        crate::folders::move_meeting(&w.svc, last, Some(kept)).unwrap();
        if damaged {
            let db = earshot_store::DataDir::new(w.dir.0.join("data")).db_path();
            let c = rusqlite::Connection::open(db).unwrap();
            let changed = c
                .execute(
                    "UPDATE folder SET name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                    rusqlite::params![broken.as_uuid().as_bytes().to_vec()],
                )
                .unwrap();
            assert_eq!(changed, 1);
        }
        let listed: Vec<EntityId> = crate::folders::list(&w.svc)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        let (map, count) = crate::folders::meeting_folders_counted_in(&w.svc.store()).unwrap();
        let of_first = crate::folders::folder_of(&w.svc, first)
            .unwrap()
            .map(|f| f.id);
        assert_eq!(count, usize::from(damaged));
        assert_eq!(map.get(&last), Some(&kept));
        if damaged {
            assert_eq!(listed, vec![kept]);
            assert_eq!(map.get(&first), None);
            assert_eq!(of_first, None);
        } else {
            assert_eq!(listed, vec![kept, broken]);
            assert_eq!(map.get(&first), Some(&broken));
            assert_eq!(of_first, Some(broken));
        }
    }
}

/// Renaming a person everywhere renames their voice in every meeting, the one whose meeting row is
/// damaged included: a write that must reach every row of an entity never selects its meetings
/// through the lenient list reader (which would leave the old name there for good).
#[test]
fn meeting_list_damaged_row_never_fails_a_person_rename() {
    for damaged in [true, false] {
        let w = World::new("rename-damaged");
        let (first, middle, last) = w.three(false);
        let person =
            EntityId::from_bytes([0x01, 0x94, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 1, 7]);
        {
            let clock = Arc::clone(&w.svc.clock);
            let mut db = w.svc.store();
            let (store, hlc) = db.parts();
            store
                .put_person(Person {
                    id: person,
                    meta: SyncMeta::new(hlc, &*clock),
                    display_name: "Invented Corvina".into(),
                    is_me: false,
                    merged_into: None,
                })
                .unwrap();
            for m in [first, middle, last] {
                let mut s = store
                    .speakers(m)
                    .unwrap()
                    .into_iter()
                    .find(|s| s.display_name == "Alice")
                    .unwrap();
                s.person_id = Some(person);
                s.display_name = "Invented Corvina".into();
                s.meta.touch(hlc, &*clock);
                store.put_speaker(s).unwrap();
            }
        }
        if damaged {
            w.damage(middle);
        }
        let rewritten = crate::edit::rename_person(&w.svc, person, "Invented Halvard").unwrap();
        assert_eq!(rewritten, 3);
        for m in [first, middle, last] {
            let names: Vec<String> = w
                .svc
                .store()
                .speakers(m)
                .unwrap()
                .into_iter()
                .map(|s| s.display_name)
                .collect();
            assert!(names.contains(&"Invented Halvard".to_owned()), "{names:?}");
            assert!(!names.contains(&"Invented Corvina".to_owned()), "{names:?}");
        }
    }
}

/// A speaker row that does not read (another voice's, which could as well be the person's):
/// renaming the person everywhere fails closed with `RenameBlockedByDamage` and writes nothing,
/// neither the person row nor any speaker row. Control: with no damage every row is renamed.
#[test]
fn meeting_list_damaged_speaker_row_refuses_a_person_rename_and_writes_nothing() {
    for damaged in [true, false] {
        let w = World::new("rename-damaged-speaker");
        let (first, middle, last) = w.three(false);
        let person =
            EntityId::from_bytes([0x01, 0x94, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 2, 7]);
        let other_voice;
        {
            let clock = Arc::clone(&w.svc.clock);
            let mut db = w.svc.store();
            let (store, hlc) = db.parts();
            store
                .put_person(Person {
                    id: person,
                    meta: SyncMeta::new(hlc, &*clock),
                    display_name: "Invented Corvina".into(),
                    is_me: false,
                    merged_into: None,
                })
                .unwrap();
            for m in [first, middle, last] {
                let mut s = store
                    .speakers(m)
                    .unwrap()
                    .into_iter()
                    .find(|s| s.display_name == "Alice")
                    .unwrap();
                s.person_id = Some(person);
                s.display_name = "Invented Corvina".into();
                s.meta.touch(hlc, &*clock);
                store.put_speaker(s).unwrap();
            }
            other_voice = store
                .speakers(middle)
                .unwrap()
                .into_iter()
                .find(|s| s.display_name == "Me")
                .unwrap()
                .id;
        }
        if damaged {
            let db = earshot_store::DataDir::new(w.dir.0.join("data")).db_path();
            let c = rusqlite::Connection::open(db).unwrap();
            let changed = c
                .execute(
                    "UPDATE speaker SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                    rusqlite::params![other_voice.as_uuid().as_bytes().to_vec()],
                )
                .unwrap();
            assert_eq!(changed, 1);
        }
        let out = crate::edit::rename_person(&w.svc, person, "Invented Halvard");
        let want = if damaged {
            assert!(
                matches!(
                    out,
                    Err(crate::RuntimeError::Edit(
                        crate::edit::EditError::RenameBlockedByDamage(p)
                    )) if p == person
                ),
                "{out:?}"
            );
            "Invented Corvina"
        } else {
            assert_eq!(out.unwrap(), 3);
            "Invented Halvard"
        };
        let db = w.svc.store();
        assert_eq!(db.person(person).unwrap().unwrap().display_name, want);
        for m in [first, last] {
            let names: Vec<String> = db
                .speakers(m)
                .unwrap()
                .into_iter()
                .map(|s| s.display_name)
                .collect();
            assert!(names.contains(&want.to_owned()), "{names:?}");
        }
    }
}

/// The recovery at start still answers for the readable interrupted meeting.
#[test]
fn meeting_list_damaged_row_never_stops_recovery() {
    for damaged in [true, false] {
        let w = World::new("recover-damaged");
        let (first, _, _) = w.three(damaged);
        {
            let mut db = w.svc.store();
            let mut m = db.meeting(first).unwrap().unwrap();
            m.duration_ms = None;
            db.put_meeting(m).unwrap();
        }
        let report = crate::recover::recover_interrupted(&w.svc);
        assert_eq!(report.list_failed, None);
        assert_eq!(report.nothing_saved, vec![first], "{report:?}");
    }
}

/// An export of every meeting exports the readable ones and counts the damaged one.
#[test]
fn meeting_list_damaged_row_never_fails_an_export_of_every_meeting() {
    for damaged in [true, false] {
        let w = World::new("export-damaged");
        let (first, middle, last) = w.three(damaged);
        let vault = w.dir.0.join("vault");
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        let batch = crate::export::export_meetings(
            &w.svc,
            &vault,
            crate::export::ExportSelection::All,
            &ExportOptions::default(),
            &mut ChaCha8Rng::seed_from_u64(9),
        )
        .unwrap();
        assert_eq!(batch.damaged, usize::from(damaged));
        let mut ids: Vec<EntityId> = batch
            .results
            .iter()
            .map(|(m, r)| {
                assert!(r.is_ok(), "{r:?}");
                *m
            })
            .collect();
        ids.sort();
        let mut want = vec![first, last];
        if !damaged {
            want.push(middle);
        }
        want.sort();
        assert_eq!(ids, want);
    }
}
