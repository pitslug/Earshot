use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde::Serialize;

use super::*;

fn rng(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(seed)
}

fn device(n: u8) -> DeviceId {
    DeviceId::from_bytes([n; 16])
}

// ---------------------------------------------------------------- Clock

#[test]
fn manual_clock_is_set_and_advanced_explicitly() {
    let c = ManualClock::new(1_000);
    assert_eq!(c.now_unix_ms(), 1_000);
    c.advance_ms(250);
    assert_eq!(c.now_unix_ms(), 1_250);
    c.set_ms(10);
    assert_eq!(c.now_unix_ms(), 10);
}

// ---------------------------------------------------------------- EntityId

#[test]
fn entity_id_is_uuid_v7_with_the_clock_timestamp() {
    let clock = ManualClock::new(1_700_000_000_123);
    let id = EntityId::generate(&clock, &mut rng(1));
    let u = id.as_uuid();
    assert_eq!(u.get_version_num(), 7);
    assert_eq!(u.get_variant(), uuid::Variant::RFC4122);
    assert_eq!(id.timestamp_ms(), 1_700_000_000_123);
}

#[test]
fn entity_id_is_deterministic_for_a_seed_and_differs_across_seeds() {
    let clock = ManualClock::new(42);
    let a = EntityId::generate(&clock, &mut rng(7));
    let b = EntityId::generate(&clock, &mut rng(7));
    let c = EntityId::generate(&clock, &mut rng(8));
    assert_eq!(a, b);
    assert_ne!(a, c);
    // Successive ids from one RNG differ even within one millisecond.
    let mut r = rng(7);
    assert_ne!(
        EntityId::generate(&clock, &mut r),
        EntityId::generate(&clock, &mut r)
    );
}

#[test]
fn entity_ids_sort_by_creation_time() {
    let clock = ManualClock::new(1_000);
    let mut r = rng(3);
    let early = EntityId::generate(&clock, &mut r);
    clock.advance_ms(1);
    let late = EntityId::generate(&clock, &mut r);
    assert!(early < late);
    assert!(early.to_string() < late.to_string());
}

#[test]
fn entity_id_round_trips_through_text_and_serde() {
    let id = EntityId::generate(&ManualClock::new(5), &mut rng(5));
    let s = id.to_string();
    assert_eq!(s.len(), 36);
    assert_eq!(s.parse::<EntityId>().unwrap(), id);
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, format!("\"{s}\""));
    assert_eq!(serde_json::from_str::<EntityId>(&json).unwrap(), id);
}

// ---------------------------------------------------------------- HLC
// Kulkarni et al., "Logical Physical Clocks" (2014), Fig. 5.

#[test]
fn hlc_follows_physical_time_when_it_advances() {
    let clock = ManualClock::new(100);
    let mut hlc = Hlc::new(device(1));
    let a = hlc.now(&clock);
    assert_eq!((a.wall_ms, a.logical), (100, 0));
    clock.advance_ms(5);
    let b = hlc.now(&clock);
    assert_eq!((b.wall_ms, b.logical), (105, 0));
    assert_eq!(b.node, device(1));
}

#[test]
fn hlc_counts_when_physical_time_stalls_or_goes_backwards() {
    let clock = ManualClock::new(100);
    let mut hlc = Hlc::new(device(1));
    let a = hlc.now(&clock);
    let b = hlc.now(&clock);
    clock.set_ms(50); // wall clock stepped back
    let c = hlc.now(&clock);
    assert_eq!((b.wall_ms, b.logical), (100, 1));
    assert_eq!((c.wall_ms, c.logical), (100, 2));
    assert!(a < b && b < c);
}

#[test]
fn hlc_receive_takes_the_max_and_stays_ahead_of_the_remote() {
    let clock = ManualClock::new(100);
    let mut hlc = Hlc::new(device(1));
    hlc.now(&clock); // (100, 0)

    // Remote ahead of both: l = l_m, c = c_m + 1.
    let remote = HlcTimestamp {
        wall_ms: 200,
        logical: 4,
        node: device(2),
    };
    let r = hlc.observe(remote, &clock);
    assert_eq!((r.wall_ms, r.logical), (200, 5));
    assert!(r > remote);

    // Remote equal to local wall: c = max(c, c_m) + 1.
    let remote = HlcTimestamp {
        wall_ms: 200,
        logical: 9,
        node: device(2),
    };
    let r = hlc.observe(remote, &clock);
    assert_eq!((r.wall_ms, r.logical), (200, 10));

    // Remote behind local: c = c + 1.
    let remote = HlcTimestamp {
        wall_ms: 150,
        logical: 99,
        node: device(2),
    };
    let r = hlc.observe(remote, &clock);
    assert_eq!((r.wall_ms, r.logical), (200, 11));

    // Physical time overtakes everything: c = 0.
    clock.set_ms(300);
    let r = hlc.observe(remote, &clock);
    assert_eq!((r.wall_ms, r.logical), (300, 0));
}

#[test]
fn hlc_timestamps_are_totally_ordered_with_the_node_as_tiebreak() {
    let a = HlcTimestamp {
        wall_ms: 1,
        logical: 0,
        node: device(1),
    };
    let b = HlcTimestamp {
        node: device(2),
        ..a
    };
    let c = HlcTimestamp { logical: 1, ..a };
    let d = HlcTimestamp { wall_ms: 2, ..a };
    assert!(a < b && b < c && c < d);
}

#[test]
fn hlc_counter_overflow_advances_the_logical_wall_time() {
    let clock = ManualClock::new(100);
    let mut hlc = Hlc::new(device(1));
    let remote = HlcTimestamp {
        wall_ms: 100,
        logical: u32::MAX,
        node: device(2),
    };
    let r = hlc.observe(remote, &clock);
    assert!(r > remote);
    assert_eq!((r.wall_ms, r.logical), (101, 0));
}

// ---------------------------------------------------------------- SyncMeta

#[test]
fn sync_meta_tracks_creation_updates_and_tombstones() {
    let clock = ManualClock::new(1_000);
    let mut hlc = Hlc::new(device(9));
    let mut meta = SyncMeta::new(&mut hlc, &clock);
    assert_eq!(meta.created_at, meta.updated_at);
    assert!(!meta.deleted);
    assert_eq!(meta.origin_device_id, device(9));

    let created = meta.created_at;
    clock.advance_ms(10);
    meta.touch(&mut hlc, &clock);
    assert!(meta.updated_at > created);
    assert_eq!(meta.created_at, created);

    let before = meta.updated_at;
    meta.mark_deleted(&mut hlc, &clock);
    assert!(meta.deleted);
    assert!(meta.updated_at > before);
}

// ---------------------------------------------------------------- SyncClass

#[derive(Serialize)]
struct TestNote {
    id: EntityId,
    meta: SyncMeta,
    text: String,
}

impl SyncClass for TestNote {
    type Class = SyncableClass;
}

impl SyncSerialize for TestNote {
    const ENTITY: &'static str = "test_note";
    fn sync_id(&self) -> EntityId {
        self.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

#[derive(Serialize)]
struct TestVoiceprint {
    id: EntityId,
    meta: SyncMeta,
}

impl SyncClass for TestVoiceprint {
    type Class = SyncableOptInClass;
}

impl SyncSerialize for TestVoiceprint {
    const ENTITY: &'static str = "test_voiceprint";
    fn sync_id(&self) -> EntityId {
        self.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

struct TestAudio;

impl SyncClass for TestAudio {
    type Class = NeverSyncClass;
}

fn class_of<T: SyncClass>() -> SyncClassKind {
    <T::Class as ClassMarker>::KIND
}

fn is_syncable<T: Syncable>() {}
fn is_opt_in<T: SyncableOptIn>() {}
fn is_never<T: NeverSync>() {}

#[test]
fn sync_class_markers_follow_the_declared_class() {
    assert_eq!(class_of::<TestNote>(), SyncClassKind::Syncable);
    assert_eq!(class_of::<TestVoiceprint>(), SyncClassKind::SyncableOptIn);
    assert_eq!(class_of::<TestAudio>(), SyncClassKind::NeverSync);
    is_syncable::<TestNote>();
    is_opt_in::<TestVoiceprint>();
    is_never::<TestAudio>();
}

#[test]
fn syncable_types_serialise_through_the_sync_trait() {
    let clock = ManualClock::new(1_000);
    let mut hlc = Hlc::new(device(1));
    let note = TestNote {
        id: EntityId::generate(&clock, &mut rng(1)),
        meta: SyncMeta::new(&mut hlc, &clock),
        text: "hi".into(),
    };
    let env = sync_envelope(&note);
    assert_eq!(env.entity, "test_note");
    assert_eq!(env.class, SyncClassKind::Syncable);
    assert_eq!(env.id, note.id);
    let json = serde_json::to_value(&env).unwrap();
    assert_eq!(json["entity"], "test_note");
    assert_eq!(json["record"]["text"], "hi");

    let vp = TestVoiceprint {
        id: EntityId::generate(&clock, &mut rng(2)),
        meta: SyncMeta::new(&mut hlc, &clock),
    };
    assert_eq!(sync_envelope(&vp).class, SyncClassKind::SyncableOptIn);
}

#[test]
fn hlc_resumed_from_a_high_water_mark_never_goes_backwards() {
    // A device restarts with its wall clock behind the last stored timestamp.
    let node = device(1);
    let hw = HlcTimestamp {
        wall_ms: 5_000,
        logical: 7,
        node: device(2),
    };
    let clock = ManualClock::new(1_000);
    let mut hlc = Hlc::resume(node, hw);
    let t = hlc.now(&clock);
    assert!(t > hw, "{t:?} must follow {hw:?}");
    assert_eq!((t.wall_ms, t.logical, t.node), (5_000, 8, node));
    // A fresh clock would have gone backwards.
    assert!(Hlc::new(node).now(&clock) < hw);
}

#[test]
fn entity_ids_within_one_millisecond_are_not_creation_ordered_but_hlc_is() {
    // Pins the documented ordering contract of `EntityId`: same-millisecond ids sort by
    // their random bits; the HLC `created_at` gives creation order.
    let clock = ManualClock::new(1_700_000_000_000);
    let mut r = rng(3);
    let mut hlc = Hlc::new(device(1));
    let rows: Vec<(EntityId, SyncMeta)> = (0..32)
        .map(|_| {
            (
                EntityId::generate(&clock, &mut r),
                SyncMeta::new(&mut hlc, &clock),
            )
        })
        .collect();
    assert!(
        rows.iter()
            .all(|(id, _)| id.timestamp_ms() == 1_700_000_000_000)
    );
    assert!(
        rows.windows(2).any(|w| w[1].0 < w[0].0),
        "same-ms ids are not in creation order"
    );
    assert!(
        rows.windows(2)
            .all(|w| w[1].1.created_at > w[0].1.created_at),
        "HLC created_at is strictly increasing in creation order"
    );
}
