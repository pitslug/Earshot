//! `vp_readout` (T12.71, evaluation only): how the shipped voiceprint setting and T12.67's
//! threshold pick (0.56) name the voices of the owner's own meetings. Counts and scores only.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example vp_readout -- \
//!     --db <earshot.db> --into fixtures/private/real-meetings/<run>
//! ```
//!
//! The data, measures and rule are docs/voiceprint-real-meetings.md, committed before this
//! tool was written. The store is copied into `--into` (which must lie inside a
//! `fixtures/private/real-meetings/` folder) and the copy is opened read-only; the original is
//! never opened. No audio is read and no model runs: every score comes from the final pass's
//! stored speaker identities (`speaker-identities/<meeting>`) and the stored voiceprints, and
//! each voice is named by the final pass's own one-voice rule (`post::voice_match`).
//!
//! It prints counts only. `readout.json` in `--into` holds per-voice scores under labels
//! `m<k>` / `v<k>`: no id, name or text. Errors never quote an id, a name or a value.

#[path = "store_parts/copy.rs"]
mod copy;

use std::collections::BTreeMap;
use std::path::PathBuf;

use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::EntityId;
use earshot_core::model::{Meeting, MeetingRepo, Speaker};
use earshot_core::voiceprint::{
    InMemoryVoiceprintRepo, Person, Voiceprint, VoiceprintRepo, calibrated_margin,
    calibrated_threshold, resolve_person,
};
use earshot_runtime::post::voice_match;
use earshot_store::ReadOnlyStore;
use earshot_store::identities::{SpeakerIdentity, StoredIdentities, decode, record_key};
use serde::Serialize;
use serde_json::json;

/// T12.67's threshold pick on AMI dev meeting audio (docs/voiceprint-meetings.md), the
/// setting compared with the shipped one.
const PICKED_THRESHOLD: f32 = 0.56;
/// Rule step 1: the confirmed, scorable voices needed for a verdict.
const MIN_CONFIRMED: usize = 10;

/// Who the owner said a voice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    /// Linked by the owner (an edit or a saved voice): the person is the truth.
    Confirmed,
    /// Linked by a final pass and never touched.
    Unconfirmed,
    /// No person.
    NoName,
}

/// What a setting does with one voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Named {
    /// The voice's owner (confirmed), or the person the pass gave it (unconfirmed).
    Owner,
    /// Someone else.
    Other,
    /// Nobody.
    Nobody,
}

/// One voice, as written to `readout.json`.
#[derive(Debug, Clone, Serialize)]
struct Voice {
    label: String,
    meeting: String,
    kind: Kind,
    /// The score against the voice's own person, its own enrolment left out (`None` when
    /// that person has no voiceprint left, or the voice has no person).
    own_score: Option<f32>,
    /// Best and runner-up person scores (left out as above).
    best: Option<f32>,
    runner_up: Option<f32>,
    /// Whether the best person is the voice's own.
    best_is_own: bool,
    shipped: Named,
    picked: Named,
}

/// Voices left out, by reason (counts only).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
struct Skipped {
    meetings_without_record: usize,
    unreadable_records: usize,
    rows_gone: usize,
    ignored: usize,
    invalid_or_uncalibrated: usize,
    /// A saved voice whose enrolment could not be found in its person's voiceprint.
    leave_out_not_found: usize,
}

#[derive(Debug, Clone, Serialize)]
struct Readout {
    shipped_threshold: f32,
    margin: f32,
    picked_threshold: f32,
    voices: Vec<Voice>,
    skipped: Skipped,
}

/// Rule counts over confirmed, scorable voices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct Outcomes {
    correct: usize,
    wrong: usize,
    miss: usize,
}

fn outcomes(voices: &[Voice], pick: impl Fn(&Voice) -> Named) -> Outcomes {
    let mut o = Outcomes::default();
    for v in voices.iter().filter(|v| scorable(v)) {
        match pick(v) {
            Named::Owner => o.correct += 1,
            Named::Other => o.wrong += 1,
            Named::Nobody => o.miss += 1,
        }
    }
    o
}

fn scorable(v: &Voice) -> bool {
    v.kind == Kind::Confirmed && v.own_score.is_some()
}

/// The rule of docs/voiceprint-real-meetings.md, steps 1 to 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    NotEnoughVoices,
    LosesNames,
    NoGain,
    Raise,
}

fn verdict(voices: &[Voice]) -> Verdict {
    let n = voices.iter().filter(|v| scorable(v)).count();
    if n < MIN_CONFIRMED {
        return Verdict::NotEnoughVoices;
    }
    let s = outcomes(voices, |v| v.shipped);
    let t = outcomes(voices, |v| v.picked);
    let lost = voices
        .iter()
        .filter(|v| scorable(v) && v.shipped == Named::Owner && v.picked != Named::Owner)
        .count();
    if lost > 0 {
        Verdict::LosesNames
    } else if t.wrong >= s.wrong {
        Verdict::NoGain
    } else {
        Verdict::Raise
    }
}

/// Own-score bands of confirmed, scorable voices: below the shipped threshold, between the
/// two, at or above the pick.
fn bands(voices: &[Voice], shipped: f32) -> [usize; 3] {
    let mut b = [0; 3];
    for s in voices
        .iter()
        .filter(|v| scorable(v))
        .filter_map(|v| v.own_score)
    {
        let k = if s < shipped {
            0
        } else if s < PICKED_THRESHOLD {
            1
        } else {
            2
        };
        b[k] += 1;
    }
    b
}

fn unit(v: &[f32]) -> Option<Vec<f32>> {
    let n = v
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    (n > 0.0 && n.is_finite()).then(|| v.iter().map(|&x| (f64::from(x) / n) as f32).collect())
}

/// The voiceprints of `model` with `centroid` taken out of `owner`'s (leave one out), in a
/// snapshot repo. Returns `None` when no live voiceprint of the owner could hold it.
fn without_enrolment(
    persons: &[Person],
    prints: &[Voiceprint],
    owner: EntityId,
    linked: EntityId,
    centroid: &Embedding,
) -> Option<InMemoryVoiceprintRepo> {
    let u = unit(&centroid.vector)?;
    let mut prints = prints.to_vec();
    // Enrolment wrote to the person the row was linked to; a later merge moves nothing, so
    // the print is the linked person's or, failing that, the owner's.
    let at = prints
        .iter()
        .position(|p| !p.meta.deleted && p.person_id == linked && p.sum.len() == u.len())
        .or_else(|| {
            prints
                .iter()
                .position(|p| !p.meta.deleted && p.person_id == owner && p.sum.len() == u.len())
        })?;
    let p = &mut prints[at];
    if p.count == 0 {
        return None;
    }
    for (s, x) in p.sum.iter_mut().zip(&u) {
        *s -= x;
    }
    p.count -= 1;
    if p.count == 0 {
        prints.remove(at);
    }
    Some(snapshot(persons, &prints))
}

fn snapshot(persons: &[Person], prints: &[Voiceprint]) -> InMemoryVoiceprintRepo {
    let mut s = InMemoryVoiceprintRepo::new();
    for p in persons {
        let _ = s.put_person(p.clone());
    }
    for v in prints {
        let _ = s.put_voiceprint(v.clone());
    }
    s
}

/// Per resolved live person, their best voiceprint score for `e` in `repo`.
fn person_scores(
    repo: &InMemoryVoiceprintRepo,
    e: &Embedding,
) -> Result<BTreeMap<EntityId, f32>, String> {
    let mut out = BTreeMap::new();
    for v in repo
        .voiceprints(&e.model)
        .map_err(|_| "reading voiceprints")?
    {
        if v.meta.deleted {
            continue;
        }
        let Some(p) = resolve_person(repo, v.person_id).map_err(|_| "resolving a person")? else {
            continue;
        };
        let Ok(s) = v.score(e) else { continue };
        let x = out.entry(p.id).or_insert(f32::NEG_INFINITY);
        *x = x.max(s);
    }
    Ok(out)
}

fn readout(store: &ReadOnlyStore) -> Result<Readout, String> {
    let persons = store.persons().map_err(|_| "reading persons")?;
    let me_ids: Vec<EntityId> = persons
        .iter()
        .filter(|p| p.is_me && p.is_live())
        .map(|p| p.id)
        .collect();
    let is_me = |id: EntityId| me_ids.contains(&id);
    let mut meetings: Vec<Meeting> = store
        .meetings()
        .map_err(|_| "reading meetings")?
        .into_iter()
        .filter(|m| !m.meta.deleted)
        .collect();
    meetings.sort_by_key(|m| (m.started_unix_ms, m.id));
    let mut model: Option<(EmbeddingModelId, f32, f32, Vec<Voiceprint>)> = None;
    let mut voices = Vec::new();
    let mut skipped = Skipped::default();
    for (mk, m) in meetings.iter().enumerate() {
        let label_m = format!("m{}", mk + 1);
        let Some(bytes) = store
            .local_value(&record_key(m.id))
            .map_err(|_| format!("{label_m}: reading its speaker record"))?
        else {
            skipped.meetings_without_record += 1;
            continue;
        };
        let entries: Vec<SpeakerIdentity> = match decode(&bytes) {
            StoredIdentities::Record(r) => r.speakers,
            StoredIdentities::Unreadable(_) => {
                skipped.unreadable_records += 1;
                continue;
            }
        };
        let rows: Vec<Speaker> = store
            .speakers(m.id)
            .map_err(|_| format!("{label_m}: reading its speakers"))?;
        for entry in entries {
            let Some(row) = rows
                .iter()
                .find(|r| r.id == entry.speaker && !r.meta.deleted)
            else {
                skipped.rows_gone += 1;
                continue;
            };
            if entry.ignored {
                skipped.ignored += 1;
                continue;
            }
            let c = &entry.centroid;
            let (Ok(threshold), true) = (calibrated_threshold(&c.model), c.is_valid()) else {
                skipped.invalid_or_uncalibrated += 1;
                continue;
            };
            if model.as_ref().is_none_or(|(id, ..)| *id != c.model) {
                let prints = store
                    .voiceprints(&c.model)
                    .map_err(|_| "reading voiceprints")?;
                model = Some((
                    c.model.clone(),
                    threshold,
                    calibrated_margin(&c.model).unwrap_or(0.0),
                    prints,
                ));
            }
            let Some((_, threshold, margin, prints)) = model.as_ref() else {
                continue;
            };
            // Who the voice is: a marked "Me" row is the user; else the linked person.
            let me_row = row.role == SpeakerRole::Me;
            let linked = if me_row {
                me_ids.first().copied()
            } else {
                row.person_id
            };
            let owner = match linked {
                Some(p) => resolve_person(store, p)
                    .map_err(|_| "resolving a person")?
                    .map(|p| p.id),
                None => None,
            };
            let kind = match owner {
                None => Kind::NoName,
                Some(_) if me_row || entry.enrolled || entry.edited(row) => Kind::Confirmed,
                Some(_) => Kind::Unconfirmed,
            };
            let repo = match (kind, entry.enrolled, owner, linked) {
                (Kind::Confirmed, true, Some(o), Some(l)) => {
                    match without_enrolment(&persons, prints, o, l, c) {
                        Some(r) => r,
                        None => {
                            skipped.leave_out_not_found += 1;
                            continue;
                        }
                    }
                }
                _ => snapshot(&persons, prints),
            };
            let scores = person_scores(&repo, c)?;
            let mut ranked: Vec<(EntityId, f32)> = scores.iter().map(|(p, s)| (*p, *s)).collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let own_score = owner.and_then(|o| scores.get(&o).copied());
            let name = |th: f32| -> Result<Named, String> {
                let (found, _) = voice_match(&repo, c, th, *margin, &is_me)
                    .map_err(|_| "matching a voice".to_owned())?;
                // Nobody but a Me row is named as the user (`post::name_speakers`).
                let found = found.map(|(p, _)| p).filter(|p| me_row || !is_me(*p));
                Ok(match (found, owner) {
                    (None, _) => Named::Nobody,
                    (Some(p), Some(o)) if p == o => Named::Owner,
                    (Some(_), _) => Named::Other,
                })
            };
            voices.push(Voice {
                label: format!("v{}", voices.len() + 1),
                meeting: label_m.clone(),
                kind,
                own_score,
                best: ranked.first().map(|r| r.1),
                runner_up: ranked.get(1).map(|r| r.1),
                best_is_own: owner.is_some() && ranked.first().map(|r| r.0) == owner,
                shipped: name(*threshold)?,
                picked: name(PICKED_THRESHOLD)?,
            });
        }
    }
    let (shipped_threshold, margin) = model.map_or((f32::NAN, f32::NAN), |(_, t, m, _)| (t, m));
    Ok(Readout {
        shipped_threshold,
        margin,
        picked_threshold: PICKED_THRESHOLD,
        voices,
        skipped,
    })
}

fn count(voices: &[Voice], kind: Kind, pick: impl Fn(&Voice) -> Named, n: Named) -> usize {
    voices
        .iter()
        .filter(|v| v.kind == kind && pick(v) == n)
        .count()
}

fn summary(r: &Readout) -> serde_json::Value {
    let v = &r.voices;
    let of = |k: Kind| v.iter().filter(|x| x.kind == k).count();
    json!({
        "task": "T12.71",
        "rule": "docs/voiceprint-real-meetings.md",
        "shipped": {"threshold": r.shipped_threshold, "margin": r.margin},
        "picked": {"threshold": r.picked_threshold, "margin": r.margin},
        "voices": {
            "confirmed": of(Kind::Confirmed),
            "confirmed_scorable": v.iter().filter(|x| scorable(x)).count(),
            "unconfirmed": of(Kind::Unconfirmed),
            "no_name": of(Kind::NoName),
        },
        "confirmed_shipped": outcomes(v, |x| x.shipped),
        "confirmed_picked": outcomes(v, |x| x.picked),
        "own_score_bands": {
            "below_shipped": bands(v, r.shipped_threshold)[0],
            "between": bands(v, r.shipped_threshold)[1],
            "at_or_above_picked": bands(v, r.shipped_threshold)[2],
        },
        "unconfirmed_kept_name": {
            "shipped": count(v, Kind::Unconfirmed, |x| x.shipped, Named::Owner),
            "picked": count(v, Kind::Unconfirmed, |x| x.picked, Named::Owner),
        },
        "no_name_given_a_name": {
            "shipped": count(v, Kind::NoName, |x| x.shipped, Named::Other),
            "picked": count(v, Kind::NoName, |x| x.picked, Named::Other),
        },
        "skipped": r.skipped,
        "verdict": verdict(v),
        "limit": "a voice saved and later re-transcribed cannot be left out of its voiceprint",
    })
}

struct Args {
    db: PathBuf,
    into: PathBuf,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let (mut db, mut into) = (None, None);
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(val()?)),
            "--into" => into = Some(PathBuf::from(val()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    match (db, into) {
        (Some(db), Some(into)) => Ok(Args { db, into }),
        _ => Err("needs --db and --into".into()),
    }
}

fn run(a: &Args) -> Result<serde_json::Value, String> {
    if !copy::inside_private_tree(&a.into) {
        return Err("--into must lie inside a fixtures/private/real-meetings/ folder".into());
    }
    let store = copy::copy_and_open(&a.db, &a.into, &[])?;
    let r = readout(&store)?;
    let s = summary(&r);
    let file = json!({"summary": s, "voices": r.voices});
    let out = a.into.join("readout.json");
    std::fs::write(
        &out,
        serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("writing {}: {e}", out.display()))?;
    Ok(s)
}

fn main() {
    match parse_args(std::env::args().skip(1)).and_then(|a| run(&a).map(|s| (a, s))) {
        Ok((a, s)) => {
            println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
            println!(
                "vp_readout: per-voice scores in {}",
                a.into.join("readout.json").display()
            );
        }
        Err(e) => {
            eprintln!("vp_readout: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::capture::TrackId;
    use earshot_core::identity::{DeviceId, Hlc, ManualClock};
    use earshot_core::model::Stamp;
    use earshot_core::prompt::Template;
    use earshot_core::voiceprint::{EnrolmentConfig, Stamp as VpStamp, enroll};
    use earshot_store::Store;
    use earshot_store::identities::{IdentityRecord, encode};
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MODEL: &str = "campplus-en-voxceleb@357a834f702b";

    fn temp(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "earshot-vp-readout-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A unit vector in 8 dimensions at `deg` degrees from axis 0 towards axis `axis`.
    fn voice(axis: usize, deg: f64) -> Embedding {
        let mut v = vec![0.0_f32; 8];
        v[0] = deg.to_radians().cos() as f32;
        v[axis] = deg.to_radians().sin() as f32;
        Embedding::new(EmbeddingModelId::new(MODEL), v)
    }

    struct World {
        clock: ManualClock,
        hlc: Hlc,
        rng: ChaCha8Rng,
    }

    impl World {
        fn new() -> Self {
            World {
                clock: ManualClock::new(1_800_000_000_000),
                hlc: Hlc::new(DeviceId::from_bytes([7; 16])),
                rng: ChaCha8Rng::seed_from_u64(712),
            }
        }
    }

    struct Built {
        db: PathBuf,
        alex: EntityId,
    }

    /// An invented store. Persons Alex and Blair (and the user, with no print). Alex's print
    /// is enrolled from axis 1 at 0 degrees plus the saved voice of meeting 1; Blair's from
    /// axis 2.
    fn build(dir: &std::path::Path) -> Built {
        let db = dir.join("earshot.db");
        let mut store = Store::open(&db).unwrap();
        let mut w = World::new();
        let mk_person = |w: &mut World, store: &mut Store, name: &str, me: bool| {
            let mut s = VpStamp {
                clock: &w.clock,
                hlc: &mut w.hlc,
                rng: &mut w.rng,
            };
            let p = Person::new(name, me, &mut s);
            store.put_person(p.clone()).unwrap();
            p.id
        };
        let alex = mk_person(&mut w, &mut store, "Alex Invented", false);
        let blair = mk_person(&mut w, &mut store, "Blair Invented", false);
        let _me = mk_person(&mut w, &mut store, "Me", true);
        let saved = voice(1, 30.0);
        let enrol = |w: &mut World, store: &mut Store, p: EntityId, e: &[Embedding]| {
            let mut s = VpStamp {
                clock: &w.clock,
                hlc: &mut w.hlc,
                rng: &mut w.rng,
            };
            enroll(
                store,
                p,
                e,
                std::time::Duration::from_secs(30),
                &EnrolmentConfig::default(),
                &mut s,
            )
            .unwrap();
        };
        // Alex: one clean print plus the saved voice of meeting 1.
        enrol(&mut w, &mut store, alex, &[voice(1, 0.0), saved.clone()]);
        enrol(&mut w, &mut store, blair, &[voice(2, 90.0)]);

        let mut st = Stamp {
            clock: &w.clock,
            hlc: &mut w.hlc,
            rng: &mut w.rng,
        };
        let meeting = |store: &mut Store, s: &mut Stamp<'_, ManualClock, ChaCha8Rng>, n: u64| {
            let m = Meeting::new(
                "An invented call",
                1_800_000_000_000 + n,
                0,
                Template::General,
                s,
            );
            store.put_meeting(m.clone()).unwrap();
            m.id
        };
        // Meeting 1: Alex's saved voice (confirmed), a voice the user left unnamed.
        let m1 = meeting(&mut store, &mut st, 1);
        let a1 = Speaker::new(
            m1,
            SpeakerRole::Remote,
            "Alex Invented",
            Some(alex),
            &mut st,
        );
        let n1 = Speaker::new(m1, SpeakerRole::Remote, "Speaker 2", None, &mut st);
        store.put_speaker(a1.clone()).unwrap();
        store.put_speaker(n1.clone()).unwrap();
        // Meeting 2: Alex linked by the user (an edit), a pass-given Blair never touched, and
        // an ignored voice.
        let m2 = meeting(&mut store, &mut st, 2);
        let a2 = Speaker::new(
            m2,
            SpeakerRole::Remote,
            "Alex Invented",
            Some(alex),
            &mut st,
        );
        let b2 = Speaker::new(
            m2,
            SpeakerRole::Remote,
            "Blair Invented",
            Some(blair),
            &mut st,
        );
        let i2 = Speaker::new(m2, SpeakerRole::Remote, "Speaker 3", None, &mut st);
        for s in [&a2, &b2, &i2] {
            store.put_speaker(s.clone()).unwrap();
        }
        let ident = |row: &Speaker, c: Embedding, edited: bool, enrolled: bool, ignored: bool| {
            SpeakerIdentity {
                speaker: row.id,
                track: TrackId::Reference,
                centroid: c,
                given_name: row.display_name.clone(),
                given_person: row.person_id,
                given_stamp: Some(row.meta.updated_at),
                user_edited: edited,
                enrolled,
                ignored,
            }
        };
        let rec1 = IdentityRecord {
            speakers: vec![
                ident(&a1, saved, true, true, false),
                ident(&n1, voice(3, 80.0), false, false, false),
            ],
        };
        let rec2 = IdentityRecord {
            speakers: vec![
                // 55 degrees from Alex's clean print: cos 0.57 before any leave-out.
                ident(&a2, voice(1, 55.0), true, false, false),
                ident(&b2, voice(2, 89.0), false, false, false),
                ident(&i2, voice(4, 10.0), false, false, true),
            ],
        };
        store
            .set_local_value(&record_key(m1), &encode(&rec1))
            .unwrap();
        store
            .set_local_value(&record_key(m2), &encode(&rec2))
            .unwrap();
        drop(store);
        Built { db, alex }
    }

    #[test]
    fn vp_readout_classifies_voices_and_leaves_a_saved_voice_out_of_its_print() {
        let dir = temp("classify");
        let b = build(&dir);
        let store = ReadOnlyStore::open(&b.db).unwrap();
        let r = readout(&store).unwrap();
        assert_eq!(r.picked_threshold, 0.56);
        assert_eq!(r.skipped.ignored, 1);
        assert_eq!(r.voices.len(), 4);
        let by = |m: &str, k: Kind| {
            r.voices
                .iter()
                .filter(|v| v.meeting == m && v.kind == k)
                .cloned()
                .collect::<Vec<_>>()
        };
        // Meeting 1's saved voice: with its own enrolment taken out, Alex's print is the
        // clean axis-1 voice at 0 degrees, so the own score is cos 30 degrees, not more.
        let saved = &by("m1", Kind::Confirmed)[0];
        let own = saved.own_score.unwrap();
        assert!(
            (own - 30.0_f64.to_radians().cos() as f32).abs() < 1e-4,
            "{own}"
        );
        assert_eq!((saved.shipped, saved.picked), (Named::Owner, Named::Owner));
        // Meeting 2's Alex (not saved: nothing left out) scores against the whole print.
        let linked = &by("m2", Kind::Confirmed)[0];
        let whole = {
            let mut s = [0.0_f32; 8];
            for e in [voice(1, 0.0), voice(1, 30.0)] {
                for (a, x) in s.iter_mut().zip(e.vector) {
                    *a += x;
                }
            }
            let n = s.iter().map(|x| x * x).sum::<f32>().sqrt();
            let c = voice(1, 55.0).vector;
            s.iter().zip(&c).map(|(a, b)| a / n * b).sum::<f32>()
        };
        assert!((linked.own_score.unwrap() - whole).abs() < 1e-4);
        assert_eq!(by("m2", Kind::Unconfirmed).len(), 1);
        assert_eq!(by("m1", Kind::NoName).len(), 1);
        let _ = b.alex;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn v(kind: Kind, own: Option<f32>, shipped: Named, picked: Named) -> Voice {
        Voice {
            label: String::new(),
            meeting: String::new(),
            kind,
            own_score: own,
            best: own,
            runner_up: None,
            best_is_own: true,
            shipped,
            picked,
        }
    }

    #[test]
    fn vp_readout_rule_needs_ten_voices_no_lost_name_and_one_fewer_wrong_name() {
        use Named::*;
        let ok = || v(Kind::Confirmed, Some(0.7), Owner, Owner);
        // Nine confirmed voices: not enough, whatever else holds.
        let mut few: Vec<Voice> = (0..8).map(|_| ok()).collect();
        few.push(v(Kind::Confirmed, Some(0.3), Other, Nobody));
        few.push(v(Kind::Unconfirmed, Some(0.5), Other, Nobody));
        assert_eq!(verdict(&few), Verdict::NotEnoughVoices);
        // A voice with no own score does not count toward the ten.
        let mut unscorable: Vec<Voice> = (0..9).map(|_| ok()).collect();
        unscorable.push(v(Kind::Confirmed, None, Nobody, Nobody));
        assert_eq!(verdict(&unscorable), Verdict::NotEnoughVoices);
        // Ten, nothing changes: no gain.
        let same: Vec<Voice> = (0..10).map(|_| ok()).collect();
        assert_eq!(verdict(&same), Verdict::NoGain);
        // One wrong name removed, nothing lost: raise.
        let mut gain = same.clone();
        gain.push(v(Kind::Confirmed, Some(0.3), Other, Nobody));
        assert_eq!(verdict(&gain), Verdict::Raise);
        // The same gain but one right name lost: refused.
        let mut loss = gain.clone();
        loss.push(v(Kind::Confirmed, Some(0.5), Owner, Nobody));
        assert_eq!(verdict(&loss), Verdict::LosesNames);
        // A right name turned into a wrong one also counts as lost.
        let mut swap = gain;
        swap.push(v(Kind::Confirmed, Some(0.5), Owner, Other));
        assert_eq!(verdict(&swap), Verdict::LosesNames);
        // Unconfirmed and no-name voices never decide.
        let mut noise: Vec<Voice> = (0..10).map(|_| ok()).collect();
        noise.push(v(Kind::NoName, None, Other, Nobody));
        noise.push(v(Kind::Unconfirmed, Some(0.5), Owner, Nobody));
        assert_eq!(verdict(&noise), Verdict::NoGain);
    }

    #[test]
    fn vp_readout_bands_split_at_both_thresholds() {
        use Named::*;
        let vs = [
            v(Kind::Confirmed, Some(0.40), Nobody, Nobody),
            v(Kind::Confirmed, Some(0.462), Owner, Nobody),
            v(Kind::Confirmed, Some(0.559), Owner, Nobody),
            v(Kind::Confirmed, Some(0.56), Owner, Owner),
            v(Kind::Unconfirmed, Some(0.50), Owner, Nobody),
        ];
        assert_eq!(bands(&vs, 0.462), [1, 2, 1]);
    }

    #[test]
    fn vp_readout_refuses_a_folder_outside_the_private_tree_and_writes_nothing_there() {
        let dir = temp("outside");
        let b = build(&dir);
        let outside = dir.join("run-out");
        let err = run(&Args {
            db: b.db.clone(),
            into: outside.clone(),
        })
        .unwrap_err();
        assert!(err.contains("fixtures/private/real-meetings"), "{err}");
        assert!(!outside.exists());
        // Inside the tree: the copy, readout.json, no id or name in the summary or file.
        let into = dir
            .join("fixtures")
            .join("private")
            .join("real-meetings")
            .join("r");
        let before = std::fs::read(&b.db).unwrap();
        let s = run(&Args {
            db: b.db.clone(),
            into: into.clone(),
        })
        .unwrap();
        assert_eq!(
            std::fs::read(&b.db).unwrap(),
            before,
            "the original is untouched"
        );
        let file = std::fs::read_to_string(into.join("readout.json")).unwrap();
        for text in [s.to_string(), file] {
            assert!(!text.contains("Invented"), "a name leaked");
            assert!(!text.contains(&b.alex.to_string()), "an id leaked");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
