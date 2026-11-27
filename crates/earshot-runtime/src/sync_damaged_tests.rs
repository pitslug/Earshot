//! T14.57 (docs/sync-spec.md 16.9): the runtime's guarded paths with a damaged row. Damage is
//! made by SQL from a second connection; every name is invented; each test states its control.

use std::sync::Arc;

use earshot_core::diar::EmbeddingModelId;
use earshot_core::identity::{EntityId, SyncMeta};
use earshot_core::model::DictionaryRepo;
use earshot_core::voiceprint::{
    Person, SampleRecord, Voiceprint, VoiceprintRepo, VoiceprintSample,
};

use crate::RuntimeError;
use crate::Services;
use crate::edit::EditError;
use crate::testutil::{TempDir, services};

const MODEL: &str = "invented-embedder@2";

fn pid(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x94, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 9])
}

struct World {
    svc: Services,
    db: std::path::PathBuf,
    _dir: TempDir,
}

impl World {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let (svc, _) = services(&dir);
        let db = earshot_store::DataDir::new(dir.0.join("data")).db_path();
        Self { svc, db, _dir: dir }
    }

    fn person(&self, n: u8, name: &str, is_me: bool, into: Option<u8>) {
        let clock = Arc::clone(&self.svc.clock);
        let mut db = self.svc.store();
        let (store, hlc) = db.parts();
        store
            .put_person(Person {
                id: pid(n),
                meta: SyncMeta::new(hlc, &*clock),
                display_name: name.into(),
                is_me,
                merged_into: into.map(pid),
            })
            .unwrap();
    }

    fn print(&self, n: u8, person: u8, count: u32) {
        let clock = Arc::clone(&self.svc.clock);
        let mut db = self.svc.store();
        let (store, hlc) = db.parts();
        let sum = if count == 0 {
            vec![0.0, 0.0]
        } else {
            vec![0.6, 0.8]
        };
        store
            .put_voiceprint(Voiceprint::from_stored(
                pid(n),
                SyncMeta::new(hlc, &*clock),
                pid(person),
                EmbeddingModelId::new(MODEL),
                sum,
                count,
                u64::from(count) * 5_000,
            ))
            .unwrap();
    }

    fn own_sample(&self, n: u8, print: u8, person: u8) {
        let clock = Arc::clone(&self.svc.clock);
        let mut db = self.svc.store();
        let (store, hlc) = db.parts();
        store
            .put_sample(
                VoiceprintSample::from_stored(SampleRecord {
                    id: pid(n),
                    meta: SyncMeta::new(hlc, &*clock),
                    voiceprint_id: Some(pid(print)),
                    person_id: Some(pid(person)),
                    model: Some(EmbeddingModelId::new(MODEL)),
                    vector: vec![1.0, 0.0],
                    speech_ms: Some(6_000),
                    meeting_id: None,
                    speaker_id: None,
                })
                .unwrap(),
            )
            .unwrap();
    }

    fn sql(&self, sql: &str, n: u8) {
        let c = rusqlite::Connection::open(&self.db).unwrap();
        c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        let changed = c
            .execute(sql, rusqlite::params![pid(n).as_uuid().as_bytes().to_vec()])
            .unwrap();
        assert_eq!(changed, 1, "{sql}");
    }

    fn print_live(&self, n: u8) -> bool {
        let c = rusqlite::Connection::open(&self.db).unwrap();
        c.query_row(
            "SELECT deleted FROM voiceprint WHERE id = ?1",
            rusqlite::params![pid(n).as_uuid().as_bytes().to_vec()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 0
    }
}

/// Test 4 (e) and its control: Remove still refuses a merged-in member whose `is_me` reads
/// true, and goes ahead when it reads false. (Test 4 (d), a member whose `is_me` does not read,
/// cannot be stored: `person.is_me` is a STRICT `INTEGER NOT NULL` column and every integer
/// reads as a bool, so `PersonUnreadable` is the defensive arm; its copy is pinned by the
/// desktop's test.)
#[test]
fn edit_remove_person_damaged_a_member_who_is_the_user_still_refuses() {
    for is_me in [true, false] {
        let w = World::new("remove-damaged-member");
        w.person(10, "Invented Rowan", false, None);
        w.person(11, "Invented Sasha", is_me, Some(10));
        w.print(30, 11, 1);
        let out = crate::edit::remove_person(&w.svc, pid(10));
        if is_me {
            assert!(
                matches!(out, Err(RuntimeError::Edit(EditError::PersonIsTheUser(_)))),
                "{out:?}"
            );
            assert!(w.print_live(30), "nothing was written");
        } else {
            out.unwrap();
            assert!(!w.print_live(30), "the member's voice went with it");
        }
    }
}

/// Test 4 (f) and 16: a speaker row elsewhere whose `person_id` is 15 bytes and a person row
/// elsewhere with a name that is not UTF-8: the People list lists everyone else and says rows
/// were left out, Forget voice and Remove both succeed. Control: without the damage the list
/// says nothing was left out.
#[test]
fn edit_remove_person_damaged_rows_elsewhere_never_stop_the_people_page() {
    for damaged in [true, false] {
        let w = World::new("remove-damaged-elsewhere");
        w.person(10, "Invented Rowan", false, None);
        w.person(12, "Invented Tamsin", false, None);
        w.print(30, 10, 1);
        let m = crate::testutil::meeting(&w.svc, "Invented planning", 7);
        let speaker = crate::view::MeetingView::load_named(&w.svc.store(), m)
            .unwrap()
            .unwrap()
            .speakers[0]
            .id;
        if damaged {
            let c = rusqlite::Connection::open(&w.db).unwrap();
            c.execute(
                "UPDATE speaker SET person_id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                rusqlite::params![speaker.as_uuid().as_bytes().to_vec()],
            )
            .unwrap();
            w.sql(
                "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                12,
            );
        }
        let list = crate::edit::list_persons(&w.svc).unwrap();
        assert_eq!(list.damaged_skipped > 0, damaged, "{list:?}");
        assert!(list.iter().any(|p| p.id == pid(10)));
        assert_eq!(list.iter().any(|p| p.id == pid(12)), !damaged);
        let forgot = crate::edit::forget_person_voices(&w.svc, pid(10)).unwrap();
        assert_eq!(forgot.voices, 1);
        crate::edit::remove_person(&w.svc, pid(10)).unwrap();
        assert!(
            !crate::edit::list_persons(&w.svc)
                .unwrap()
                .iter()
                .any(|p| p.id == pid(10))
        );
    }
}

/// Test 15a (the `id` case) and 15: Forget my voice with a person row whose `id` is 15 bytes
/// (its `is_me` true): the trigger fires, that person cannot be reached and the answer is
/// incomplete; the user's own voice is forgotten all the same. Control: with the row's `id`
/// repaired the answer is complete and its voice is forgotten too.
#[test]
fn my_voice_damaged_a_person_whose_id_does_not_read_makes_it_incomplete() {
    for repaired in [false, true] {
        let w = World::new("my-voice-damaged-id");
        w.person(1, "Me", true, None);
        w.print(31, 1, 1);
        w.own_sample(41, 31, 1);
        w.person(5, "Me", true, None);
        w.print(35, 5, 1);
        if !repaired {
            w.sql(
                "UPDATE person SET id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                5,
            );
        }
        let out = crate::my_voice::forget_my_voice(&w.svc).unwrap();
        assert_eq!(out.incomplete, !repaired, "{out:?}");
        assert!(!w.print_live(31), "the user's own voice is forgotten");
        assert_eq!(
            !w.print_live(35),
            repaired,
            "the unreachable person's voice is kept"
        );
    }
}

/// Test 15a (the `Own` samples): a person the user's own sample names is a starting point only
/// when its `is_me` does not read false, so a print of someone else named through a sample's
/// `voiceprint_id` survives. Here every `is_me` reads (STRICT), so the trigger is a person whose
/// `id` does not read; W (`is_me` false) keeps its print, and the user's sample on W's print is
/// erased with the user's voice.
#[test]
fn my_voice_damaged_an_own_sample_on_someone_elses_print_takes_only_the_sample() {
    let w = World::new("my-voice-damaged-own");
    w.person(1, "Me", true, None);
    w.print(31, 1, 1);
    w.person(7, "Invented Wren", false, None);
    w.print(37, 7, 1);
    w.own_sample(47, 37, 1);
    w.person(8, "Invented Xavi", false, None);
    w.sql(
        "UPDATE person SET id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
        8,
    );
    let out = crate::my_voice::forget_my_voice(&w.svc).unwrap();
    assert!(
        out.incomplete,
        "a person whose id does not read may be the user"
    );
    assert!(!w.print_live(31));
    assert!(
        w.print_live(37),
        "someone else's print is never taken through a link"
    );
    let c = rusqlite::Connection::open(&w.db).unwrap();
    let erased: i64 = c
        .query_row(
            "SELECT deleted FROM voiceprint_sample WHERE id = ?1",
            rusqlite::params![pid(47).as_uuid().as_bytes().to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(erased, 1, "the user's own sample is erased");
}

/// Test 20 and 8a: with a damaged Dictionary word row every page edit is refused with
/// `Damaged` and writes nothing, and the reconcile reports it instead of failing (with sync off
/// and on). Control: with the row repaired the same edits succeed.
#[test]
fn sync_damaged_dictionary_edits_refuse_while_a_word_row_does_not_read() {
    for (repaired, syncing) in [(false, false), (false, true), (true, false), (true, true)] {
        let w = World::new("dictionary-damaged");
        if syncing {
            // Dictionary sync on (sync itself on; the Dictionary switch is on by default).
            let c = rusqlite::Connection::open(&w.db).unwrap();
            c.execute(
                "INSERT INTO local_state (key, value) VALUES (?1, ?2)",
                rusqlite::params![crate::sync::SYNC_ON_KEY, b"on".to_vec()],
            )
            .unwrap();
        }
        crate::vocabulary::add_entry(&w.svc, "Quillmarsh").unwrap();
        crate::vocabulary::add_entry(&w.svc, "Ostrelby").unwrap();
        let rows = w.svc.store().dictionary_words().unwrap();
        let row = rows.iter().find(|r| r.word == "Ostrelby").unwrap().id;
        let damage = if repaired {
            "UPDATE dictionary_word SET word = word WHERE id = ?1"
        } else {
            "UPDATE dictionary_word SET word = CAST(X'FF00FE' AS TEXT) WHERE id = ?1"
        };
        let c = rusqlite::Connection::open(&w.db).unwrap();
        c.execute(damage, rusqlite::params![row.as_uuid().as_bytes().to_vec()])
            .unwrap();
        let before = w
            .svc
            .store()
            .local_value(crate::vocabulary::VOCABULARY_KEY)
            .unwrap();
        let added = crate::vocabulary::add_entry(&w.svc, "Velmora");
        let removed = crate::vocabulary::remove_entry(&w.svc, "Quillmarsh");
        let reconciled = crate::vocabulary::reconcile(&w.svc).unwrap();
        if repaired {
            added.unwrap();
            assert!(removed.unwrap());
            assert!(matches!(reconciled, crate::vocabulary::Reconciled::Made(_)));
        } else {
            assert!(
                matches!(
                    added,
                    Err(crate::vocabulary::SetVocabularyError::Damaged { syncing: s }) if s == syncing
                ),
                "{added:?}"
            );
            assert!(
                matches!(
                    removed,
                    Err(crate::vocabulary::SetVocabularyError::Damaged { syncing: s }) if s == syncing
                ),
                "{removed:?}"
            );
            assert!(matches!(
                reconciled,
                crate::vocabulary::Reconciled::Unread { skipped: 1 }
            ));
            assert_eq!(
                w.svc
                    .store()
                    .local_value(crate::vocabulary::VOCABULARY_KEY)
                    .unwrap(),
                before,
                "the stored list is kept"
            );
        }
    }
}

/// Test 10: the start-up repairs finish with damaged rows present and write their records. The
/// summary-seen start skips a meeting whose row does not read; the ignore copy skips a damaged
/// speaker row and still writes `ignore-copied`, and a second start runs nothing. Control: the
/// same starts on a store without the damage write the same records.
#[test]
fn sync_damaged_start_up_repairs_finish_and_mark_themselves_done() {
    for damaged in [true, false] {
        let w = World::new("start-up-damaged");
        let m = crate::testutil::meeting(&w.svc, "Invented review", 3);
        let other = crate::testutil::meeting(&w.svc, "Invented retro", 4);
        if damaged {
            let c = rusqlite::Connection::open(&w.db).unwrap();
            c.execute(
                "UPDATE meeting SET started_unix_ms = -1 WHERE id = ?1",
                rusqlite::params![other.as_uuid().as_bytes().to_vec()],
            )
            .unwrap();
            c.execute(
                "UPDATE speaker SET role = 'bogus' WHERE meeting_id = ?1",
                rusqlite::params![m.as_uuid().as_bytes().to_vec()],
            )
            .unwrap();
        }
        crate::summary_seen::mark_existing_once(&w.svc).unwrap();
        assert!(
            w.svc
                .store()
                .local_value(crate::summary_seen::SEEN_START_KEY)
                .unwrap()
                .is_some()
        );
        assert_eq!(crate::ignore_copy::copy_once(&w.svc).unwrap(), 0);
        assert!(crate::ignore_copy::copied(&w.svc.store()).unwrap());
        assert_eq!(crate::ignore_copy::copy_once(&w.svc).unwrap(), 0, "once");
    }
}
