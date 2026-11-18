//! T12.77b3: every way a voice sample is erased leaves no trace on disk, and pulled print
//! tombstones are final and erase what they hold (`cargo test -p earshot-store`;
//! docs/voice-samples.md 6, 7.4 and 8). Every name is invented.

mod common;

use std::time::Duration;

use common::TempDir;
use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::identity::{DeviceId, EntityId, Hlc, HlcTimestamp, ManualClock};
use earshot_core::model::Stamp;
use earshot_core::voiceprint::{
    EnrolmentConfig, Person, SampleOrigin, Voiceprint, VoiceprintRepo, effective, enroll,
    enroll_sample, take_back,
};
use earshot_store::{ApplyRules, Incoming, Store, StoreError, SyncKinds, SyncedRow};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

const HERE: DeviceId = DeviceId::from_bytes([4; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);

struct World {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl World {
    fn stamp(&mut self) -> Stamp<'_, ManualClock, ChaCha8Rng> {
        self.clock.advance_ms(1);
        Stamp {
            clock: &self.clock,
            hlc: &mut self.hlc,
            rng: &mut self.rng,
        }
    }
}

/// Whether `needle` occurs in the database file or its WAL.
fn on_disk(db: &std::path::Path, needle: &[u8]) -> bool {
    ["", "-wal", "-journal"].iter().any(|suffix| {
        std::fs::read(format!("{}{suffix}", db.display()))
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
    })
}

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("invented-embedder@0")
}

/// A store with a person whose print has a base (an enrolment of today's shape) and one
/// sample from a meeting voice. The needles: a run of the sample's vector, of the base's
/// vector, and the origin's meeting and speaker ids (written nowhere else).
struct Setup {
    _dir: TempDir,
    db: std::path::PathBuf,
    store: Store,
    w: World,
    person: EntityId,
    print: EntityId,
    meeting: EntityId,
    speaker: EntityId,
    sample_needle: Vec<u8>,
    base_needle: Vec<u8>,
}

fn setup(tag: &str) -> Setup {
    let dir = TempDir::new(tag);
    let db = dir.path().join("earshot.db");
    let mut store = Store::open(&db).unwrap();
    assert!(store.secure_delete().unwrap());
    let mut w = World {
        clock: ManualClock::new(1_800_000_000_000),
        hlc: Hlc::new(HERE),
        rng: ChaCha8Rng::seed_from_u64(77),
    };
    let ada = Person::new("An invented colleague", false, &mut w.stamp());
    store.put_person(ada.clone()).unwrap();
    let base: Vec<f32> = (0..64).map(|i| 0.01 + i as f32 * 0.0137).collect();
    let print = enroll(
        &mut store,
        ada.id,
        &[Embedding::new(model(), base)],
        Duration::from_secs(12),
        &EnrolmentConfig::default(),
        &mut w.stamp(),
    )
    .unwrap();
    let voice: Vec<f32> = (0..64).map(|i| 0.9 - i as f32 * 0.0091).collect();
    let (meeting, speaker) = (
        EntityId::from_bytes([0x5a; 16]),
        EntityId::from_bytes([0x5b; 16]),
    );
    let origin = SampleOrigin::Meeting { meeting, speaker };
    enroll_sample(
        &mut store,
        ada.id,
        &[Embedding::new(model(), voice)],
        Duration::from_secs(5),
        origin,
        &EnrolmentConfig::default(),
        &mut w.stamp(),
    )
    .unwrap();
    let s = store.samples_from(meeting, speaker).unwrap().remove(0);
    let sample_needle: Vec<u8> = s.vector().unwrap()[10..16]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    let v = store.voiceprints_of(ada.id).unwrap().remove(0);
    // The fold without the sample is the stored base.
    let base_needle: Vec<u8> = effective(&v, &[]).sum()[10..16]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    for n in [&sample_needle, &base_needle] {
        assert!(on_disk(&db, n), "sanity: on disk before erasing");
    }
    assert!(on_disk(&db, &meeting.as_uuid().as_bytes()[..]));
    Setup {
        _dir: dir,
        db,
        store,
        w,
        person: ada.id,
        print,
        meeting,
        speaker,
        sample_needle,
        base_needle,
    }
}

impl Setup {
    /// The sample's vector and origin ids are gone from the file and the WAL.
    fn assert_sample_gone(&self) {
        assert!(!on_disk(&self.db, &self.sample_needle), "the sample vector");
        for id in [self.meeting, self.speaker] {
            assert!(
                !on_disk(&self.db, &id.as_uuid().as_bytes()[..]),
                "an origin id"
            );
        }
    }
}

#[test]
fn store_sample_erasure_take_back_leaves_no_trace_on_disk() {
    let mut s = setup("erase-take-back");
    // A second connection open across the erasure, so a closing checkpoint cannot hide a
    // missing one.
    let _other = rusqlite::Connection::open(&s.db).unwrap();
    let (meeting, speaker) = (s.meeting, s.speaker);
    let w = &mut s.w;
    let n = s
        .store
        .transaction(|tx| {
            take_back(tx, meeting, speaker, None, &mut w.hlc, &w.clock).map_err(|e| {
                StoreError::Repo(earshot_core::model::RepoError::Storage(e.to_string()))
            })
        })
        .unwrap();
    assert_eq!(n, 1);
    s.store.flush_erasures().unwrap();
    s.assert_sample_gone();
    assert!(
        on_disk(&s.db, &s.base_needle),
        "the rest of the print stays"
    );
}

#[test]
fn store_sample_erasure_forget_leaves_no_trace_on_disk() {
    let mut s = setup("erase-forget");
    let _other = rusqlite::Connection::open(&s.db).unwrap();
    let n = s
        .store
        .forget_voiceprints(s.person, &mut s.w.hlc, &s.w.clock)
        .unwrap();
    assert_eq!((n.voiceprints, n.samples), (1, 1));
    s.assert_sample_gone();
    assert!(!on_disk(&s.db, &s.base_needle));
}

#[test]
fn store_sample_erasure_remove_person_leaves_no_trace_on_disk() {
    let mut s = setup("erase-remove");
    let _other = rusqlite::Connection::open(&s.db).unwrap();
    let r = s
        .store
        .remove_person(s.person, &mut s.w.hlc, &s.w.clock)
        .unwrap();
    assert_eq!((r.voiceprints_forgotten, r.samples_forgotten), (1, 1));
    s.assert_sample_gone();
    assert!(!on_disk(&s.db, &s.base_needle));
}

fn rules() -> ApplyRules<'static> {
    fn free(_: EntityId) -> bool {
        false
    }
    ApplyRules {
        kinds: SyncKinds::ALL,
        leased: &free,
        now_ms: 1_800_000_100_000,
        after: None,
    }
}

fn incoming(seq: u64, row: SyncedRow) -> Incoming {
    let stamp = row.meta().updated_at;
    let mut key = [0u8; 16];
    key.copy_from_slice(&row.id().as_uuid().as_bytes()[..]);
    key[0] ^= 0xFF;
    Incoming {
        key,
        seq,
        stamp,
        body: vec![0xA5; 40],
        row: Some(row),
    }
}

/// The print as another computer sends it, stamped at `wall_ms`, live or a tombstone that
/// still carries its vector (stored zeroed whatever it holds).
fn print_from_there(here: &Voiceprint, wall_ms: u64, deleted: bool) -> Voiceprint {
    let mut meta = *here.meta();
    meta.updated_at = HlcTimestamp {
        wall_ms,
        logical: 0,
        node: THERE,
    };
    meta.deleted = deleted;
    Voiceprint::from_stored(
        here.id(),
        meta,
        here.person_id(),
        here.model().clone(),
        effective(here, &[]).sum().to_vec(),
        1,
        12_000,
    )
}

#[test]
fn store_sample_erasure_a_pulled_print_tombstone_is_stored_zeroed_and_erases_its_samples() {
    let mut s = setup("erase-pulled");
    let _other = rusqlite::Connection::open(&s.db).unwrap();
    let here = s.store.voiceprints_of(s.person).unwrap().remove(0);
    assert_eq!(here.id(), s.print);
    let gone = print_from_there(&here, 1_800_000_050_000, true);
    let mut hlc = s.store.hlc(HERE).unwrap();
    let a = s
        .store
        .apply_remote(
            &[incoming(1, SyncedRow::Voiceprint(gone))],
            &rules(),
            &mut hlc,
            &s.w.clock,
        )
        .unwrap();
    assert_eq!(a.written, 1);
    // The live print it replaced and the sample of it the repair erased.
    assert_eq!(a.biometrics_forgotten, 2, "{a:?}");
    s.store.flush_erasures().unwrap();
    let v = s.store.voiceprints_of(s.person).unwrap().remove(0);
    assert!(v.meta().deleted);
    assert!(effective(&v, &[]).sum().iter().all(|x| *x == 0.0));
    assert!(s.store.samples_of(s.print).unwrap().is_empty());
    s.assert_sample_gone();
    assert!(
        !on_disk(&s.db, &s.base_needle),
        "the pulled vector is never stored"
    );
}

#[test]
fn store_sample_erasure_a_voiceprint_tombstone_is_final_against_a_later_live_copy() {
    let mut s = setup("erase-final");
    s.store
        .forget_voiceprints(s.person, &mut s.w.hlc, &s.w.clock)
        .unwrap();
    let mut hlc = s.store.hlc(HERE).unwrap();
    let tomb = s.store.voiceprints_of(s.person).unwrap().remove(0);
    let forgot_at = tomb.meta().updated_at;
    // An older live copy is ignored.
    let older = print_from_there(&tomb, forgot_at.wall_ms - 1, false);
    let a = s
        .store
        .apply_remote(
            &[incoming(1, SyncedRow::Voiceprint(older))],
            &rules(),
            &mut hlc,
            &s.w.clock,
        )
        .unwrap();
    assert_eq!((a.written, a.ignored, a.deletions_kept), (0, 1, 0), "{a:?}");
    // A later live copy (an older build that still had the print) never brings it back: the
    // tombstone is stamped again after it and queued to go out.
    let later = print_from_there(&tomb, forgot_at.wall_ms + 60_000, false);
    let later_at = later.meta().updated_at;
    let a = s
        .store
        .apply_remote(
            &[incoming(2, SyncedRow::Voiceprint(later))],
            &rules(),
            &mut hlc,
            &s.w.clock,
        )
        .unwrap();
    assert_eq!((a.written, a.deletions_kept), (0, 1), "{a:?}");
    let v = s.store.voiceprints_of(s.person).unwrap().remove(0);
    assert!(v.meta().deleted, "still forgotten");
    assert!(effective(&v, &[]).sum().iter().all(|x| *x == 0.0));
    assert!(
        v.meta().updated_at > later_at,
        "stamped again after the live copy"
    );
    assert!(
        s.store
            .outbox(1_000)
            .unwrap()
            .contains(&SyncedRow::Voiceprint(v)),
        "the tombstone goes out again"
    );
}

/// T12.77b3 review: the dead-print repair erases by id, so a damaged live sample row (one the
/// store would refuse to read) is erased too and never aborts the page.
#[test]
fn store_sample_erasure_a_damaged_sample_of_a_tombstoned_print_is_erased_not_fatal() {
    let mut s = setup("erase-damaged");
    {
        let c = rusqlite::Connection::open(&s.db).unwrap();
        let bad: Vec<u8> = [2.0f32; 64].iter().flat_map(|x| x.to_le_bytes()).collect();
        c.execute("UPDATE voiceprint_sample SET vector = ?1", [bad])
            .unwrap();
    }
    assert!(
        s.store.samples_from(s.meeting, s.speaker).is_err(),
        "sanity: damaged"
    );
    let here = s.store.voiceprints_of(s.person).unwrap().remove(0);
    let gone = print_from_there(&here, 1_800_000_050_000, true);
    let mut hlc = s.store.hlc(HERE).unwrap();
    let a = s
        .store
        .apply_remote(
            &[incoming(1, SyncedRow::Voiceprint(gone))],
            &rules(),
            &mut hlc,
            &s.w.clock,
        )
        .unwrap();
    assert_eq!((a.written, a.biometrics_forgotten), (1, 2), "{a:?}");
    s.store.flush_erasures().unwrap();
    assert!(
        s.store
            .samples_from(s.meeting, s.speaker)
            .unwrap()
            .is_empty()
    );
    let left: i64 = rusqlite::Connection::open(&s.db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM voiceprint_sample WHERE deleted = 0 OR length(vector) > 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "a tombstone with no body");
    s.assert_sample_gone();
}
