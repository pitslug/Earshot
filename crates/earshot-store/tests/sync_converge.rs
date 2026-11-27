//! T14.50: the convergence repairs (docs/sync-spec.md section 7) run only for the kinds of row
//! a page wrote, and give exactly what running every repair after every page gives
//! (`cargo test -p earshot-store sync`). Every name is invented.

mod common;

use std::time::Duration;

use common::TempDir;
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::identity::{Clock, DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{ClockDomain, Folder, Meeting, MeetingRepo, Stamp, Summary, Track};
use earshot_core::prompt::Template;
use earshot_core::summary::{SUMMARY_SCHEMA_VERSION, SummaryV1};
use earshot_core::voiceprint::{
    DEFAULT_ME_NAME, EnrolmentConfig, Person, SampleOrigin, SampleRecord, Voiceprint,
    VoiceprintRepo, VoiceprintSample, enroll, merge_persons,
};
use earshot_store::{Applied, ApplyRules, Incoming, Store, SyncKinds, SyncedRow};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);
const THIRD: DeviceId = DeviceId::from_bytes([0x33; 16]);

const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 7])
}

fn at(wall_ms: u64, logical: u32, node: DeviceId) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms,
        logical,
        node,
    }
}

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("invented-embedder@0")
}

/// A small generator with a fixed seed: the pages are varied and the same on every run.
struct Dice(u64);

impl Dice {
    fn roll(&mut self, below: u64) -> u64 {
        // Knuth's MMIX constants; the high bits are the usable ones.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.roll(n) == 0
    }

    fn pick(&mut self, from: std::ops::Range<u8>) -> u8 {
        from.start + u8::try_from(self.roll(u64::from(from.end - from.start))).unwrap()
    }

    fn stamp(&mut self, base: u64) -> HlcTimestamp {
        let node = if self.one_in(2) { THERE } else { THIRD };
        at(
            base + self.roll(40),
            u32::try_from(self.roll(3)).unwrap(),
            node,
        )
    }

    fn meta(&mut self, base: u64, deleted: bool) -> SyncMeta {
        let updated = self.stamp(base);
        SyncMeta {
            // Created at different times, so the earliest "Me" and the latest summary vary.
            created_at: at(T0 - 50 + self.roll(40), 0, updated.node),
            updated_at: updated,
            deleted,
            origin_device_id: updated.node,
        }
    }
}

const MEETINGS: std::ops::Range<u8> = 1..4;
const PERSONS: std::ops::Range<u8> = 10..17;
const SUMMARIES: std::ops::Range<u8> = 20..30;
const PRINTS: std::ops::Range<u8> = 30..37;
const SAMPLES: std::ops::Range<u8> = 40..54;

fn meeting(n: u8, meta: SyncMeta) -> Meeting {
    Meeting {
        id: id(n),
        meta,
        title: "An invented meeting".into(),
        started_unix_ms: T0,
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn person(dice: &mut Dice, n: u8, base: u64) -> Person {
    let deleted = dice.one_in(5);
    let is_me = dice.one_in(3);
    let merged_into = (!deleted && dice.one_in(4))
        .then(|| dice.pick(PERSONS))
        .filter(|other| *other != n)
        .map(id);
    Person {
        id: id(n),
        meta: dice.meta(base, deleted),
        display_name: if deleted {
            String::new()
        } else if is_me && dice.one_in(2) {
            DEFAULT_ME_NAME.into()
        } else {
            format!("Invented person {n}")
        },
        is_me,
        merged_into,
    }
}

fn summary(dice: &mut Dice, n: u8, base: u64) -> Summary {
    let deleted = dice.one_in(4);
    Summary {
        id: id(n),
        meta: dice.meta(base, deleted),
        // A summary keeps its meeting: the row's id decides it.
        meeting_id: id(MEETINGS.start + n % (MEETINGS.end - MEETINGS.start)),
        schema_version: SUMMARY_SCHEMA_VERSION,
        template: Template::General,
        model: None,
        summary: SummaryV1 {
            tldr: format!("An invented summary {n}"),
            ..SummaryV1::default()
        },
    }
}

fn print(dice: &mut Dice, n: u8, base: u64) -> Voiceprint {
    let deleted = dice.one_in(5);
    Voiceprint::from_stored(
        id(n),
        dice.meta(base, deleted),
        // A print keeps its person.
        id(PERSONS.start + n % (PERSONS.end - PERSONS.start)),
        model(),
        vec![0.0; 3],
        0,
        0,
    )
}

fn sample(dice: &mut Dice, n: u8, base: u64) -> VoiceprintSample {
    let deleted = dice.one_in(5);
    let meta = dice.meta(base, deleted);
    if deleted {
        return VoiceprintSample::erased(id(n), meta).unwrap();
    }
    let print = PRINTS.start + n % (PRINTS.end - PRINTS.start);
    VoiceprintSample::from_stored(SampleRecord {
        id: id(n),
        meta,
        voiceprint_id: Some(id(print)),
        // The print's person when written; another person's now and then (a moved sample).
        person_id: Some(id(if dice.one_in(6) {
            dice.pick(PERSONS)
        } else {
            PERSONS.start + print % (PERSONS.end - PERSONS.start)
        })),
        model: Some(model()),
        vector: vec![0.6, 0.8, 0.0],
        speech_ms: Some(4_000),
        meeting_id: Some(id(dice.pick(MEETINGS))),
        speaker_id: Some(id(200)),
    })
    .unwrap()
}

/// A row of a kind no repair reads.
fn other(dice: &mut Dice, base: u64) -> SyncedRow {
    match dice.roll(3) {
        0 => SyncedRow::Meeting(meeting(dice.pick(MEETINGS), dice.meta(base, false))),
        1 => SyncedRow::Track(Track {
            id: id(60 + dice.pick(0..3)),
            meta: dice.meta(base, false),
            meeting_id: id(dice.pick(MEETINGS)),
            kind: TrackId::Mic,
            device_name: None,
            clock_domain: ClockDomain::Replay,
        }),
        _ => SyncedRow::Folder(Folder {
            id: id(70 + dice.pick(0..3)),
            meta: dice.meta(base, false),
            name: "An invented folder".into(),
        }),
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
        entity: Some(row.table().into()),
        row: Some(row),
    }
}

fn free(_: EntityId) -> bool {
    false
}

fn rules(now_ms: u64) -> ApplyRules<'static> {
    ApplyRules {
        kinds: SyncKinds::ALL,
        leased: &free,
        now_ms,
        after: None,
    }
}

/// One device's store, written through `apply_remote` (`every == false`) or with every repair
/// after every page (`every == true`).
struct Device {
    store: Store,
    db: std::path::PathBuf,
    clock: ManualClock,
    every: bool,
    _dir: TempDir,
}

impl Device {
    fn new(tag: &str, every: bool) -> Self {
        let dir = TempDir::new(tag);
        let db = dir.path().join("earshot.db");
        Self {
            store: Store::open(&db).unwrap(),
            db,
            clock: ManualClock::new(T0 + 10_000),
            every,
            _dir: dir,
        }
    }

    /// The page's outcome, or its error's text (a page the store refuses changes nothing).
    fn apply(&mut self, page: &[Incoming]) -> Result<Applied, String> {
        let mut hlc = self.store.hlc(HERE).unwrap();
        let rules = rules(self.clock.now_unix_ms());
        if self.every {
            self.store
                .apply_remote_every_repair(page, &rules, &mut hlc, &self.clock)
        } else {
            self.store.apply_remote(page, &rules, &mut hlc, &self.clock)
        }
        .map_err(|e| e.to_string())
    }

    /// Something this computer does itself between pages, as the app does it: `seed` makes the
    /// ids it creates the same on both devices.
    fn local(&mut self, what: u64, a: u8, b: u8, seed: u64) -> String {
        let mut hlc = self.store.hlc(HERE).unwrap();
        self.clock.advance_ms(1);
        match what {
            0 => format!(
                "{:?}",
                self.store
                    .remove_person(id(a), &mut hlc, &self.clock)
                    .map_err(|e| e.to_string())
            ),
            1 => format!(
                "{:?}",
                merge_persons(&mut self.store, id(a), id(b), &mut hlc, &self.clock)
                    .map_err(|e| e.to_string())
            ),
            2 => format!(
                "{:?}",
                self.store
                    .forget_voiceprints(id(a), &mut hlc, &self.clock)
                    .map_err(|e| e.to_string())
            ),
            _ => {
                let mut rng = ChaCha8Rng::seed_from_u64(seed);
                let mut stamp = Stamp {
                    clock: &self.clock,
                    hlc: &mut hlc,
                    rng: &mut rng,
                };
                format!(
                    "{:?}",
                    enroll(
                        &mut self.store,
                        id(a),
                        &[Embedding::new(model(), vec![0.0, 0.6, 0.8])],
                        Duration::from_secs(12),
                        SampleOrigin::Own,
                        &EnrolmentConfig::default(),
                        &mut stamp,
                    )
                    .map_err(|e| e.to_string())
                )
            }
        }
    }

    /// Every row of every table, read through a connection of its own.
    fn dump(&self) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        let c = rusqlite::Connection::open(&self.db).unwrap();
        let tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(tables.iter().any(|t| t == "person") && tables.iter().any(|t| t == "sync_outbox"));
        tables
            .into_iter()
            .map(|t| {
                let mut st = c.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
                let n = st.column_count();
                let mut rows: Vec<Vec<rusqlite::types::Value>> = st
                    .query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                // The one-off identity sweep's device record (T14.49c), written by every first
                // open since, is left out so the digest still pins exactly the rows it pinned
                // before; the sweep has tests of its own (`tests/sync_sweep.rs`).
                let sweep =
                    rusqlite::types::Value::Text(earshot_store::identities::SWEEP_KEY.to_owned());
                rows.retain(|r| !(t == "local_state" && r.first() == Some(&sweep)));
                rows.sort_by_key(|r| format!("{r:?}"));
                (t, rows)
            })
            .collect()
    }
}

/// A page of one to six rows; most pages hold rows of one or two kinds, so many pages write
/// nothing a given repair reads.
fn page(dice: &mut Dice, seq: &mut u64, base: u64) -> Vec<Incoming> {
    let kinds: Vec<u64> = (0..=dice.roll(2)).map(|_| dice.roll(5)).collect();
    (0..=dice.roll(6))
        .map(|_| {
            let kind = kinds[usize::try_from(dice.roll(kinds.len() as u64)).unwrap()];
            let n = dice.pick(match kind {
                0 => PERSONS,
                1 => SUMMARIES,
                2 => PRINTS,
                _ => SAMPLES,
            });
            let row = match kind {
                0 => SyncedRow::Person(person(dice, n, base)),
                1 => SyncedRow::Summary(summary(dice, n, base)),
                2 => SyncedRow::Voiceprint(print(dice, n, base)),
                3 => SyncedRow::VoiceprintSample(sample(dice, n, base)),
                _ => other(dice, base),
            };
            *seq += 1;
            incoming(*seq, row)
        })
        .collect()
}

#[test]
fn store_sync_repairs_by_the_kinds_a_page_wrote_equal_every_repair_after_every_page() {
    let mut totals = Applied::default();
    let (mut applied_pages, mut quiet_pages) = (0, 0);
    for seed in 0..12u64 {
        let mut dice = Dice(0x5EED_0000 + seed);
        let mut gated = Device::new("converge-gated", false);
        let mut every = Device::new("converge-every", true);
        let mut seq = 0;
        // Every meeting, person and print first, live, so later rows have what they belong to.
        let mut first: Vec<Incoming> = Vec::new();
        for n in MEETINGS {
            seq += 1;
            first.push(incoming(
                seq,
                SyncedRow::Meeting(meeting(n, dice.meta(T0, false))),
            ));
        }
        for n in PERSONS {
            seq += 1;
            let mut p = person(&mut dice, n, T0);
            p.meta.deleted = false;
            p.merged_into = None;
            first.push(incoming(seq, SyncedRow::Person(p)));
        }
        for n in PRINTS {
            seq += 1;
            let p = Voiceprint::from_stored(
                id(n),
                dice.meta(T0, false),
                id(PERSONS.start + n % (PERSONS.end - PERSONS.start)),
                model(),
                vec![0.0; 3],
                0,
                0,
            );
            first.push(incoming(seq, SyncedRow::Voiceprint(p)));
        }
        assert_eq!(gated.apply(&first), every.apply(&first), "seed {seed}");
        assert_eq!(gated.dump(), every.dump(), "seed {seed}: the first page");
        for step in 0..70u64 {
            // Stamps drift upwards, so some rows are newer than the copy here and some are not.
            let base = T0 + step * 4;
            if dice.one_in(5) {
                let (what, a, b) = (dice.roll(4), dice.pick(PERSONS), dice.pick(PERSONS));
                let op_seed = seed * 1000 + step;
                assert_eq!(
                    gated.local(what, a, b, op_seed),
                    every.local(what, a, b, op_seed),
                    "seed {seed} step {step}: this computer's own write {what}"
                );
            }
            let page = page(&mut dice, &mut seq, base);
            let page = with_removal_rows(&mut dice, &mut seq, base, page);
            let (g, e) = (gated.apply(&page), every.apply(&page));
            assert_eq!(g, e, "seed {seed} step {step}: what the page did");
            assert_eq!(
                gated.dump(),
                every.dump(),
                "seed {seed} step {step}: the stores"
            );
            if let Ok(a) = g {
                applied_pages += 1;
                let touches_a_repair = page.iter().any(|inc| {
                    matches!(
                        inc.row,
                        Some(
                            SyncedRow::Person(_)
                                | SyncedRow::Summary(_)
                                | SyncedRow::Voiceprint(_)
                                | SyncedRow::VoiceprintSample(_)
                        )
                    )
                });
                if !touches_a_repair {
                    quiet_pages += 1;
                }
                totals.mes_merged += a.mes_merged;
                totals.summaries_replaced += a.summaries_replaced;
                totals.biometrics_forgotten += a.biometrics_forgotten;
                totals.written += a.written;
                totals.ignored += a.ignored;
                totals.tombstones_restamped += a.tombstones_restamped;
                totals.persons_removed_with += a.persons_removed_with;
                totals.speakers_unlinked += a.speakers_unlinked;
            }
        }
        // The outboxes agree row for row as well (the dump holds them): what goes out is the same.
        assert_eq!(
            gated.store.outbox(10_000).unwrap(),
            every.store.outbox(10_000).unwrap(),
            "seed {seed}"
        );
    }
    // The pages did exercise every repair, wrote and ignored rows, and many of them held no row
    // a repair reads: the comparison is not of two stores nothing happened to.
    assert!(applied_pages > 600, "{applied_pages} pages applied");
    assert!(
        quiet_pages > 40,
        "{quiet_pages} pages without a repair's kind"
    );
    assert!(totals.mes_merged > 5, "{totals:?}");
    assert!(totals.summaries_replaced > 5, "{totals:?}");
    assert!(totals.biometrics_forgotten > 5, "{totals:?}");
    assert!(totals.written > 500 && totals.ignored > 100, "{totals:?}");
    // T14.49a: the removal rules (docs/sync-spec.md 7A.2) ran on both stores and agreed.
    assert!(totals.tombstones_restamped > 5, "{totals:?}");
    assert!(totals.persons_removed_with > 0, "{totals:?}");
    assert!(totals.speakers_unlinked > 5, "{totals:?}");
}

const FOLDERS: std::ops::Range<u8> = 70..73;

/// T14.49a: `page` plus up to two speaker rows linked to the generated people (P3) and folder
/// rows, live or tombstones (F1), so the comparison above covers the removal rules too (P1, P2
/// and P4 act on the generated person rows). Used by that comparison only: test 12's pages stay
/// as they were.
fn with_removal_rows(
    dice: &mut Dice,
    seq: &mut u64,
    base: u64,
    mut page: Vec<Incoming>,
) -> Vec<Incoming> {
    for _ in 0..dice.roll(3) {
        let row = if dice.one_in(3) {
            let deleted = dice.one_in(3);
            let meta = dice.meta(base, deleted);
            SyncedRow::Folder(Folder {
                id: id(dice.pick(FOLDERS)),
                meta,
                name: if deleted {
                    String::new()
                } else {
                    "An invented folder".into()
                },
            })
        } else {
            let n = dice.pick(SPEAKERS);
            let deleted = dice.one_in(5);
            let person = (!dice.one_in(4)).then(|| dice.pick(PERSONS));
            let meta = dice.meta(base, deleted);
            // A speaker keeps its meeting: the row's id decides it.
            let meeting = MEETINGS.start + n % (MEETINGS.end - MEETINGS.start);
            SyncedRow::Speaker(speaker_row(n, meeting, person, meta))
        };
        *seq += 1;
        page.push(incoming(*seq, row));
    }
    page
}

/// What the gating rests on, shown by breaking it: this computer never leaves two live "Me"
/// people or two live summaries of one meeting by itself. Written here by hand (no shipped
/// code does), such rows are mended by a page that writes that kind, or by a page applied with
/// every repair, and by no other page.
#[test]
fn store_sync_a_repair_runs_only_for_a_page_that_wrote_its_kind_or_when_all_are_asked_for() {
    let mut d = Device::new("converge-kinds", false);
    let me = |n: u8, wall: u64| Person {
        id: id(n),
        meta: SyncMeta {
            created_at: at(wall, 0, HERE),
            updated_at: at(wall, 0, HERE),
            deleted: false,
            origin_device_id: HERE,
        },
        display_name: DEFAULT_ME_NAME.into(),
        is_me: true,
        merged_into: None,
    };
    d.store.put_person(me(10, T0)).unwrap();
    d.store.put_person(me(11, T0 + 1)).unwrap();
    let live_mes = |d: &Device| {
        d.store
            .persons()
            .unwrap()
            .into_iter()
            .filter(|p| p.is_me && p.is_live())
            .count()
    };
    assert_eq!(live_mes(&d), 2);
    let m = |seq: u64, wall: u64| {
        incoming(
            seq,
            SyncedRow::Meeting(meeting(
                1,
                SyncMeta {
                    created_at: at(T0, 0, THERE),
                    updated_at: at(wall, 0, THERE),
                    deleted: false,
                    origin_device_id: THERE,
                },
            )),
        )
    };
    // A page with a meeting row alone: no repair reads meetings.
    let a = d.apply(&[m(1, T0 + 5)]).unwrap();
    assert_eq!((a.written, a.mes_merged), (1, 0));
    assert_eq!(live_mes(&d), 2, "left as it was");
    // A page that writes a person row runs the "Me" repair.
    let other = Person {
        id: id(12),
        meta: SyncMeta {
            created_at: at(T0, 0, THERE),
            updated_at: at(T0 + 6, 0, THERE),
            deleted: false,
            origin_device_id: THERE,
        },
        display_name: "An invented colleague".into(),
        is_me: false,
        merged_into: None,
    };
    let a = d.apply(&[incoming(2, SyncedRow::Person(other))]).unwrap();
    assert_eq!(a.mes_merged, 1);
    assert_eq!(live_mes(&d), 1);
    assert_eq!(
        d.store.person(id(11)).unwrap().unwrap().merged_into,
        Some(id(10)),
        "the later one is merged into the earlier"
    );
    // The same again on a new store, mended by a meeting page applied with every repair.
    let mut d = Device::new("converge-every-repair", true);
    d.store.put_person(me(10, T0)).unwrap();
    d.store.put_person(me(11, T0 + 1)).unwrap();
    let a = d.apply(&[m(1, T0 + 5)]).unwrap();
    assert_eq!((a.written, a.mes_merged), (1, 1));
    assert_eq!(live_mes(&d), 1);
    assert!(d.store.meeting(id(1)).unwrap().is_some());
}

/// A speaker row of `meeting` linked to `person` (or unlinked), live or a tombstone.
fn speaker_row(
    n: u8,
    meeting: u8,
    person: Option<u8>,
    meta: SyncMeta,
) -> earshot_core::model::Speaker {
    earshot_core::model::Speaker {
        id: id(n),
        meta,
        meeting_id: id(meeting),
        role: if n.is_multiple_of(5) {
            earshot_core::diar::SpeakerRole::Me
        } else {
            earshot_core::diar::SpeakerRole::Remote
        },
        display_name: if meta.deleted {
            String::new()
        } else {
            format!("Invented speaker {n}")
        },
        person_id: person.map(id),
        ignored: n.is_multiple_of(7),
    }
}

const SPEAKERS: std::ops::Range<u8> = 80..96;

/// T14.57 test 12 (docs/sync-spec.md 16.9): with no damaged row anywhere, removals, Forget
/// voice, merges, enrolments and pages over generated stores (merges, voices, tombstoned "Me"
/// people, speaker rows linked to people and their identity entries) leave exactly the rows and
/// answers they left before T14.57. The digest was captured from the commit before the change
/// (57f0369), never from the new code.
///
/// Changed on purpose by T14.49a (docs/sync-spec.md 7A.11, "The deliberate change to section
/// 16"): the digest was `9d8564a2...ed77` until then. Per-step store dumps of these runs before
/// 7A and with it (T14.49f, compared table by table) are equal up to the first page where a 7A
/// rule acts, in every seed:
/// - seed 0: P4, a pulled live copy of a person written over the user's tombstone here;
/// - seed 1: `MeTombstone`, after this test's own `Store::remove_person` of an `is_me` person:
///   of two voice samples the page pulls, the one of that "Me" is no longer erased by rule 3a
///   but by rule 3 (its print is tombstoned) right after it, so the two erasures swap their
///   logical stamps; no other column changes;
/// - seed 2 and seed 5: P3 unlinking speaker rows from a removed person; in the same page the
///   prints and samples rule 3a erases take stamps one logical tick later, because P1's decision
///   (`tombstone_final`) observes a pulled person row's stamp once more before writing it;
/// - seed 3: P1 writing a pulled person tombstone over a later live copy here, stamped again;
/// - seed 4: `MeTombstone` again (after this test's `remove_person` of an `is_me` person): a
///   pulled sample of that "Me" stays live.
///
/// Each of those rules is pinned with literal expectations in `tests/sync_removed.rs`.
#[test]
fn store_sync_damaged_same_answers_without_damage_as_before() {
    use sha2::Digest;
    let mut all = sha2::Sha256::new();
    let (mut ops, mut removed) = (0, 0);
    for seed in 0..6u64 {
        let mut dice = Dice(0xD4A6_0000 + seed);
        let mut d = Device::new("damaged-baseline", false);
        let mut seq = 0;
        let mut first: Vec<Incoming> = Vec::new();
        for n in MEETINGS {
            seq += 1;
            first.push(incoming(
                seq,
                SyncedRow::Meeting(meeting(n, dice.meta(T0, false))),
            ));
        }
        for n in PERSONS {
            seq += 1;
            let mut p = person(&mut dice, n, T0);
            p.meta.deleted = false;
            p.merged_into = None;
            first.push(incoming(seq, SyncedRow::Person(p)));
        }
        for n in PRINTS {
            seq += 1;
            let p = Voiceprint::from_stored(
                id(n),
                dice.meta(T0, false),
                id(PERSONS.start + n % (PERSONS.end - PERSONS.start)),
                model(),
                vec![0.5, 0.5, 0.0],
                u32::try_from(dice.roll(3)).unwrap(),
                3_000,
            );
            first.push(incoming(seq, SyncedRow::Voiceprint(p)));
        }
        for n in SPEAKERS {
            seq += 1;
            let deleted = dice.one_in(5);
            let person = (!dice.one_in(4)).then(|| dice.pick(PERSONS));
            let meta = dice.meta(T0, deleted);
            let meeting = dice.pick(MEETINGS);
            first.push(incoming(
                seq,
                SyncedRow::Speaker(speaker_row(n, meeting, person, meta)),
            ));
        }
        all.update(format!("{}", d.apply(&first).is_ok()));
        // Identity entries for every meeting: some given a person, some naming a row, one
        // naming no row.
        for m in MEETINGS {
            let speakers = (0..4u8)
                .map(|k| {
                    let row = dice.pick(SPEAKERS);
                    let given_person = dice.one_in(2).then(|| id(dice.pick(PERSONS)));
                    earshot_store::identities::SpeakerIdentity {
                        speaker: id(if k == 3 { 250 } else { row }),
                        track: TrackId::Mic,
                        centroid: Embedding::new(model(), vec![0.1, 0.2, 0.3]),
                        given_name: format!("Invented given {k}"),
                        given_person,
                        given_stamp: None,
                        user_edited: false,
                        enrolled: false,
                        ignored: false,
                        linked_before: Vec::new(),
                    }
                })
                .collect();
            let record = earshot_store::identities::IdentityRecord { speakers };
            d.store
                .set_local_value(
                    &earshot_store::identities::record_key(id(m)),
                    &earshot_store::identities::encode(&record),
                )
                .unwrap();
        }
        for step in 0..60u64 {
            let base = T0 + step * 4;
            // Only the rows and whether each step succeeded are pinned: the counts reported
            // gain fields in T14.57, and the rows are what every count describes.
            if dice.one_in(3) {
                let (what, a, b) = (dice.roll(4), dice.pick(PERSONS), dice.pick(PERSONS));
                let out = d.local(what, a, b, seed * 1000 + step);
                all.update(format!("{what} {}", out.starts_with("Ok")));
                if what == 0 && out.starts_with("Ok") {
                    removed += 1;
                }
                all.update(format!("{:?}", d.dump()));
                ops += 1;
            }
            let page = page(&mut dice, &mut seq, base);
            all.update(format!("{}", d.apply(&page).is_ok()));
            all.update(format!("{:?}", d.dump()));
        }
    }
    assert!(
        ops > 60 && removed > 10,
        "{ops} local operations, {removed} removals"
    );
    let digest: String = all.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        digest, "abfe7d728d970db2a624301a987e31fc8371a4d17ba18e545214dc26146dee29",
        "the rows and answers of the generated stores"
    );
}
