//! Voiceprint tests (`cargo test -p earshot-core voiceprint`).

use std::time::Duration;

use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::identity::{DeviceId, ManualClock, SyncClassKind, sync_envelope};

struct World {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
    repo: InMemoryVoiceprintRepo,
}

impl World {
    fn new() -> Self {
        let clock = ManualClock::new(1_800_000_000_000);
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        let hlc = Hlc::new(DeviceId::generate(&clock, &mut rng));
        World {
            clock,
            hlc,
            rng,
            repo: InMemoryVoiceprintRepo::new(),
        }
    }

    fn person(&mut self, name: &str) -> EntityId {
        let p = Person::new(
            name,
            false,
            &mut stamp(&self.clock, &mut self.hlc, &mut self.rng),
        );
        self.repo.put_person(p.clone()).unwrap();
        p.id
    }

    fn enroll(
        &mut self,
        person: EntityId,
        e: &[Embedding],
        secs: u64,
    ) -> Result<EntityId, VoiceprintError> {
        self.clock.advance_ms(1000);
        enroll(
            &mut self.repo,
            person,
            e,
            Duration::from_secs(secs),
            SampleOrigin::Own,
            &EnrolmentConfig::default(),
            &mut stamp(&self.clock, &mut self.hlc, &mut self.rng),
        )
    }
}

fn stamp<'a>(
    clock: &'a ManualClock,
    hlc: &'a mut Hlc,
    rng: &'a mut ChaCha8Rng,
) -> Stamp<'a, ManualClock, ChaCha8Rng> {
    Stamp { clock, hlc, rng }
}

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("test-model@1")
}

fn emb(v: &[f32]) -> Embedding {
    Embedding::new(model(), v.to_vec())
}

#[test]
fn voiceprint_running_centroid_is_the_normalised_mean_of_unit_embeddings() {
    let mut w = World::new();
    let p = w.person("Ada");
    // Different scales must not matter: each embedding counts once, as a unit vector.
    let vp = w
        .enroll(p, &[emb(&[3.0, 0.0]), emb(&[0.0, 0.5])], 12)
        .unwrap();
    let (v, e) = live_prints_of(&w.repo, p).unwrap().remove(0);
    assert_eq!(v.id, vp);
    assert_eq!((e.count(), e.speech_ms()), (2, 12_000));
    assert_eq!((v.count, v.speech_ms), (0, 0), "the base is never added to");
    let c = e.centroid().unwrap().vector;
    let h = std::f32::consts::FRAC_1_SQRT_2;
    assert!((c[0] - h).abs() < 1e-6 && (c[1] - h).abs() < 1e-6, "{c:?}");
    // Update: the same voiceprint grows, and the centroid moves to the mean of all three.
    let created = v.meta.updated_at;
    let again = w.enroll(p, &[emb(&[1.0, 0.0])], 2).unwrap();
    assert_eq!(again, vp, "an update never creates a second voiceprint");
    let (v, e) = live_prints_of(&w.repo, p).unwrap().remove(0);
    assert_eq!((e.count(), e.speech_ms()), (3, 14_000));
    let c = e.centroid().unwrap().vector;
    let n = (4.0f32 + 1.0).sqrt();
    assert!((c[0] - 2.0 / n).abs() < 1e-6 && (c[1] - 1.0 / n).abs() < 1e-6);
    assert_eq!(v.meta.updated_at, created, "the print row is written once");
    let stamps: Vec<_> = w
        .repo
        .samples_of(vp)
        .unwrap()
        .iter()
        .map(|s| s.meta().created_at)
        .collect();
    assert!(
        stamps.len() == 3 && stamps[2] > created,
        "updates are stamped samples"
    );
}

#[test]
fn voiceprint_new_enrolment_needs_ten_seconds_and_valid_matching_embeddings() {
    let mut w = World::new();
    let p = w.person("Bo");
    assert!(matches!(
        w.enroll(p, &[emb(&[1.0, 0.0])], 9),
        Err(VoiceprintError::NotEnoughSpeech { .. })
    ));
    assert_eq!(w.enroll(p, &[], 20), Err(VoiceprintError::NoEmbeddings));
    let other = Embedding::new(EmbeddingModelId::new("other@2"), vec![1.0, 0.0]);
    assert!(matches!(
        w.enroll(p, &[emb(&[1.0, 0.0]), other], 20),
        Err(VoiceprintError::Embedding(DiarError::ModelMismatch { .. }))
    ));
    assert!(matches!(
        w.enroll(p, &[emb(&[1.0, 0.0]), emb(&[1.0, 0.0, 0.0])], 20),
        Err(VoiceprintError::Embedding(DiarError::Shape(_)))
    ));
    assert!(matches!(
        w.enroll(p, &[emb(&[0.0, 0.0])], 20),
        Err(VoiceprintError::Embedding(DiarError::Shape(_)))
    ));
    assert!(
        w.repo.voiceprints_of(p).unwrap().is_empty(),
        "failures store nothing"
    );
    let ghost = EntityId::from_bytes([7; 16]);
    assert_eq!(
        w.enroll(ghost, &[emb(&[1.0, 0.0])], 20),
        Err(VoiceprintError::UnknownPerson(ghost))
    );
}

#[test]
fn voiceprint_models_are_versioned_and_never_compared() {
    let mut w = World::new();
    let p = w.person("Cy");
    w.enroll(p, &[emb(&[1.0, 0.0])], 10).unwrap();
    let m2 = EmbeddingModelId::new("test-model@2");
    let e2 = Embedding::new(m2.clone(), vec![1.0, 0.0]);
    // A second model gets its own voiceprint for the same person.
    w.enroll(p, std::slice::from_ref(&e2), 10).unwrap();
    let prints = w.repo.voiceprints_of(p).unwrap();
    assert_eq!(prints.len(), 2);
    assert_eq!(w.repo.voiceprints(&m2).unwrap().len(), 1);
    // Matching an embedding of a third model finds nothing, even with an identical vector.
    let e3 = Embedding::new(EmbeddingModelId::new("test-model@3"), vec![1.0, 0.0]);
    assert_eq!(
        match_embedding(&w.repo, &e3, 0.0).unwrap(),
        MatchOutcome::Unknown { best: None }
    );
    // Scoring across models is an error, not a number.
    let (_, v1) = live_prints(&w.repo, &model()).unwrap().remove(0);
    assert!(matches!(
        v1.score(&e2),
        Err(VoiceprintError::Embedding(DiarError::ModelMismatch { .. }))
    ));
    // Enrolling another model's embedding beside this one's is refused, writing nothing.
    let before = w.repo.samples_of_person(p).unwrap();
    assert!(w.enroll(p, &[emb(&[1.0, 0.0]), e2], 10).is_err());
    assert_eq!(w.repo.samples_of_person(p).unwrap(), before);
}

#[test]
fn voiceprint_open_set_matching_picks_the_best_person_above_the_threshold() {
    let mut w = World::new();
    let (a, b) = (w.person("A"), w.person("B"));
    w.enroll(a, &[emb(&[1.0, 0.0, 0.0])], 10).unwrap();
    w.enroll(b, &[emb(&[0.0, 1.0, 0.0])], 10).unwrap();
    let near_a = emb(&[0.9, 0.1, 0.1]);
    match match_embedding(&w.repo, &near_a, 0.5).unwrap() {
        MatchOutcome::Known { person, score } => {
            assert_eq!(person, a);
            assert!(
                (score - near_a.cosine_similarity(&emb(&[1.0, 0.0, 0.0])).unwrap()).abs() < 1e-6
            );
        }
        other => panic!("{other:?}"),
    }
    // A stranger below the threshold is unknown, but the closest person is reported.
    let stranger = emb(&[0.2, 0.1, 1.0]);
    match match_embedding(&w.repo, &stranger, 0.5).unwrap() {
        MatchOutcome::Unknown { best: Some((p, s)) } => {
            assert_eq!(p, a);
            assert!(s < 0.5);
        }
        other => panic!("{other:?}"),
    }
    // The threshold is inclusive.
    let s = near_a.cosine_similarity(&emb(&[1.0, 0.0, 0.0])).unwrap();
    assert!(matches!(
        match_embedding(&w.repo, &near_a, s).unwrap(),
        MatchOutcome::Known { .. }
    ));
    assert!(matches!(
        match_embedding(&w.repo, &near_a, s.next_up()).unwrap(),
        MatchOutcome::Unknown { .. }
    ));
    assert!(match_embedding(&w.repo, &emb(&[0.0, 0.0, 0.0]), 0.5).is_err());
}

#[test]
fn voiceprint_merged_persons_match_as_their_target_and_deleted_prints_are_ignored() {
    let mut w = World::new();
    let (dup, keep) = (w.person("Dee (laptop)"), w.person("Dee"));
    w.enroll(dup, &[emb(&[1.0, 0.0])], 10).unwrap();
    merge_persons(&mut w.repo, dup, keep, &mut w.hlc, &w.clock).unwrap();
    assert_eq!(resolve_person(&w.repo, dup).unwrap().unwrap().id, keep);
    match match_embedding(&w.repo, &emb(&[1.0, 0.1]), 0.5).unwrap() {
        MatchOutcome::Known { person, .. } => assert_eq!(person, keep),
        other => panic!("{other:?}"),
    }
    // Merging a merged person again, or into itself, is refused.
    assert!(merge_persons(&mut w.repo, dup, keep, &mut w.hlc, &w.clock).is_err());
    assert!(merge_persons(&mut w.repo, keep, keep, &mut w.hlc, &w.clock).is_err());
    // Forgetting tombstones the voiceprint (vector cleared) and it no longer matches.
    assert_eq!(
        forget_voiceprints(&mut w.repo, dup, &mut w.hlc, &w.clock)
            .unwrap()
            .voiceprints,
        1
    );
    let v = w.repo.voiceprints_of(dup).unwrap().remove(0);
    assert!(v.meta.deleted && v.sum.iter().all(|&x| x == 0.0));
    assert_eq!(
        match_embedding(&w.repo, &emb(&[1.0, 0.1]), 0.5).unwrap(),
        MatchOutcome::Unknown { best: None }
    );
    assert_eq!(
        forget_voiceprints(&mut w.repo, dup, &mut w.hlc, &w.clock)
            .unwrap()
            .total(),
        0
    );
}

#[test]
fn voiceprint_forgetting_a_merge_target_forgets_every_merged_identity() {
    let mut w = World::new();
    let (laptop, phone, dee, other) = (
        w.person("Dee (laptop)"),
        w.person("Dee (phone)"),
        w.person("Dee"),
        w.person("Other"),
    );
    w.enroll(laptop, &[emb(&[1.0, 0.0, 0.0])], 10).unwrap();
    w.enroll(phone, &[emb(&[0.9, 0.1, 0.0])], 10).unwrap();
    w.enroll(dee, &[emb(&[0.95, 0.05, 0.0])], 10).unwrap();
    w.enroll(other, &[emb(&[0.0, 0.0, 1.0])], 10).unwrap();
    // phone -> laptop -> dee: a two-step chain ends at Dee.
    merge_persons(&mut w.repo, laptop, dee, &mut w.hlc, &w.clock).unwrap();
    let mut p = w.repo.person(phone).unwrap().unwrap();
    p.merged_into = Some(laptop);
    w.repo.put_person(p).unwrap();
    assert_eq!(resolve_person(&w.repo, phone).unwrap().unwrap().id, dee);

    let n = forget_voiceprints(&mut w.repo, dee, &mut w.hlc, &w.clock).unwrap();
    assert_eq!(
        n,
        Forgotten {
            voiceprints: 3,
            samples: 3,
            entries: 0,
            zeroed: 0,
            skipped: 0,
        },
        "Dee's own print and both merged prints, with their samples"
    );
    for id in [laptop, phone, dee] {
        let v = w.repo.voiceprints_of(id).unwrap().remove(0);
        assert!(v.meta.deleted && v.sum.iter().all(|&x| x == 0.0), "{id:?}");
    }
    // Only "Other" is left to score (orthogonal, 0.0); nothing of Dee may still match.
    assert_eq!(
        match_embedding(&w.repo, &emb(&[1.0, 0.0, 0.0]), 0.9).unwrap(),
        MatchOutcome::Unknown {
            best: Some((other, 0.0))
        },
    );
    let mut alone = w.repo.clone();
    forget_voiceprints(&mut alone, other, &mut w.hlc, &w.clock).unwrap();
    assert_eq!(
        match_embedding(&alone, &emb(&[1.0, 0.0, 0.0]), 0.9).unwrap(),
        MatchOutcome::Unknown { best: None }
    );
    // Other people are untouched.
    assert!(!w.repo.voiceprints_of(other).unwrap()[0].meta.deleted);
    assert!(matches!(
        match_embedding(&w.repo, &emb(&[0.0, 0.0, 1.0]), 0.9).unwrap(),
        MatchOutcome::Known { person, .. } if person == other
    ));
}

#[test]
fn voiceprint_invalid_rows_are_skipped_not_fatal() {
    let mut w = World::new();
    let (a, b) = (w.person("A"), w.person("B"));
    w.enroll(a, &[emb(&[1.0, 0.0])], 10).unwrap();
    // A corrupt row for B (a count but a zero sum, not tombstoned) and a wrong-dimension row.
    let mut vb = w.repo.voiceprints_of(a).unwrap().remove(0);
    vb.id = EntityId::from_bytes([8; 16]);
    vb.person_id = b;
    vb.sum = vec![0.0, 0.0];
    vb.count = 2;
    w.repo.put_voiceprint(vb.clone()).unwrap();
    let mut wide = w.repo.voiceprints_of(a).unwrap().remove(0);
    wide.id = EntityId::from_bytes([9; 16]);
    wide.sum = vec![1.0, 0.0, 0.0];
    wide.count = 1;
    w.repo.put_voiceprint(wide).unwrap();
    let m = match_embedding_with_report(&w.repo, &emb(&[1.0, 0.1]), 0.5).unwrap();
    assert!(matches!(m.outcome, MatchOutcome::Known { person, .. } if person == a));
    assert_eq!(m.skipped_invalid, 2);
    assert!(matches!(
        match_embedding(&w.repo, &emb(&[1.0, 0.1]), 0.5).unwrap(),
        MatchOutcome::Known { .. }
    ));
}

#[test]
fn voiceprint_merge_cycles_resolve_to_nobody() {
    let mut w = World::new();
    let (a, b) = (w.person("A"), w.person("B"));
    let mut pa = w.repo.person(a).unwrap().unwrap();
    let mut pb = w.repo.person(b).unwrap().unwrap();
    pa.merged_into = Some(b);
    pb.merged_into = Some(a);
    w.repo.put_person(pa).unwrap();
    w.repo.put_person(pb).unwrap();
    assert_eq!(resolve_person(&w.repo, a).unwrap(), None);
}

#[test]
fn voiceprint_me_enrolment_creates_one_me_and_reuses_it() {
    let mut w = World::new();
    let cfg = EnrolmentConfig::default();
    // Too little speech for a first enrolment: no "Me" row is left behind.
    let r = enroll_me(
        &mut w.repo,
        &[emb(&[1.0, 0.0])],
        Duration::from_secs(3),
        SampleOrigin::Own,
        &cfg,
        &mut stamp(&w.clock, &mut w.hlc, &mut w.rng),
    );
    assert!(matches!(r, Err(VoiceprintError::NotEnoughSpeech { .. })));
    assert!(
        w.repo.persons().unwrap().is_empty(),
        "a refused first enrolment writes nothing"
    );
    assert_eq!(existing_me(&w.repo).unwrap(), None);
    let (me, vp) = enroll_me(
        &mut w.repo,
        &[emb(&[1.0, 0.0])],
        Duration::from_secs(10),
        SampleOrigin::Own,
        &cfg,
        &mut stamp(&w.clock, &mut w.hlc, &mut w.rng),
    )
    .unwrap();
    let persons = w.repo.persons().unwrap();
    assert_eq!(persons.len(), 1, "{persons:?}");
    assert!(persons[0].is_me && persons[0].display_name == "Me");
    // Later "Me" speech (any amount) updates the same person and voiceprint.
    let (me2, vp2) = enroll_me(
        &mut w.repo,
        &[emb(&[0.9, 0.1])],
        Duration::from_secs(2),
        SampleOrigin::Own,
        &cfg,
        &mut stamp(&w.clock, &mut w.hlc, &mut w.rng),
    )
    .unwrap();
    assert_eq!((me2, vp2), (me, vp));
    assert_eq!(w.repo.persons().unwrap().len(), 1);
    assert_eq!(live_prints_of(&w.repo, me).unwrap()[0].1.count(), 2);
}

#[test]
fn voiceprint_sync_classes_person_syncable_voiceprint_opt_in() {
    let mut w = World::new();
    let p = w.person("E");
    w.enroll(p, &[emb(&[1.0, 0.0])], 10).unwrap();
    let person = w.repo.person(p).unwrap().unwrap();
    let vp = w.repo.voiceprints_of(p).unwrap().remove(0);
    let pe = sync_envelope(&person);
    assert_eq!((pe.entity, pe.class), ("person", SyncClassKind::Syncable));
    let ve = sync_envelope(&vp);
    assert_eq!(
        (ve.entity, ve.class),
        ("voiceprint", SyncClassKind::SyncableOptIn)
    );
}

#[test]
fn voiceprint_calibration_rule_accepts_at_most_the_target_share_of_impostors() {
    let scores: Vec<f32> = (0..40).map(|i| i as f32 / 100.0).collect(); // 0.00..0.39
    // 2.5% of 40 = 1 impostor may be accepted: the threshold sits between 0.38 and 0.39.
    let t = calibrate_threshold(&scores, 0.025);
    assert!((t - 0.385).abs() < 1e-6, "{t}");
    assert_eq!(scores.iter().filter(|&&s| s >= t).count(), 1);
    // Zero allowed: above the top score.
    let t0 = calibrate_threshold(&scores, 0.0);
    assert!(t0 > 0.39 && scores.iter().all(|&s| s < t0));
    // Ties at the cut are all rejected (never more than allowed).
    let tied = [0.5f32, 0.5, 0.5, 0.1];
    let tt = calibrate_threshold(&tied, 0.25);
    assert_eq!(tied.iter().filter(|&&s| s >= tt).count(), 0);
    // Order of the input does not matter.
    let mut rev = scores.clone();
    rev.reverse();
    assert_eq!(calibrate_threshold(&rev, 0.025), t);
}

#[test]
fn voiceprint_thresholds_exist_only_for_calibrated_models() {
    let campplus = EmbeddingModelId::new("campplus-en-voxceleb@357a834f702b");
    let t = calibrated_threshold(&campplus).unwrap();
    assert_eq!(t, CAMPPLUS_DEV_CLEAN_THRESHOLD);
    assert!(t.is_finite() && (-1.0..=1.0).contains(&t));
    // A changed model file (different hash) has no threshold until it is calibrated.
    let changed = EmbeddingModelId::new("campplus-en-voxceleb@000000000000");
    assert_eq!(
        calibrated_threshold(&changed),
        Err(VoiceprintError::NoThreshold(changed))
    );
    for c in CALIBRATED_THRESHOLDS {
        assert!(c.source.contains("dev-clean"), "{}", c.model);
        assert!(!c.source.contains("test-clean"), "{}", c.model);
    }
}

#[test]
fn voiceprint_naming_threshold_is_stricter_than_the_same_voice_threshold() {
    // T12.84: naming a voice after a stored person needs 0.56 (AMI dev meetings); the
    // same-voice threshold stays the dev-clean calibration.
    let campplus = EmbeddingModelId::new("campplus-en-voxceleb@357a834f702b");
    assert_eq!(naming_threshold(&campplus), Ok(CAMPPLUS_MEETING_THRESHOLD));
    assert_eq!(CAMPPLUS_MEETING_THRESHOLD, 0.56);
    assert_eq!(
        calibrated_threshold(&campplus),
        Ok(CAMPPLUS_DEV_CLEAN_THRESHOLD)
    );
    const { assert!(CAMPPLUS_MEETING_THRESHOLD > CAMPPLUS_DEV_CLEAN_THRESHOLD) };
    // Every model with a naming threshold has a same-voice one, and the naming one is never
    // the looser of the two.
    for c in CALIBRATED_NAMING_THRESHOLDS {
        let same = calibrated_threshold(&EmbeddingModelId::new(c.model)).unwrap();
        assert!(c.threshold >= same, "{}", c.model);
        assert!(c.source.contains("AMI dev"), "{}", c.model);
        assert!(!c.source.contains("test-clean"), "{}", c.model);
    }
    let changed = EmbeddingModelId::new("campplus-en-voxceleb@000000000000");
    assert_eq!(
        naming_threshold(&changed),
        Err(VoiceprintError::NoThreshold(changed))
    );
}

#[test]
fn voiceprint_margin_names_the_best_person_only_clear_of_the_runner_up() {
    // T12.67: A and B both above the threshold; a voice between them is named only when its
    // best person beats the runner-up by the margin. With no runner-up no margin is needed,
    // and a margin of 0 is the plain match.
    let mut w = World::new();
    let a = w.person("Ada");
    let b = w.person("Bo");
    w.enroll(a, &[emb(&[1.0, 0.0, 0.0])], 12).unwrap();
    w.enroll(b, &[emb(&[0.0, 1.0, 0.0])], 12).unwrap();
    // Cosine about 0.83 to A and 0.55 to B: 0.28 apart.
    let near_a = emb(&[0.83, 0.55, 0.0]);
    let known = |m: f32| match match_embedding_with_margin(&w.repo, &near_a, 0.5, m)
        .unwrap()
        .outcome
    {
        MatchOutcome::Known { person, .. } => Some(person),
        MatchOutcome::Unknown { best } => {
            assert_eq!(
                best.map(|(p, _)| p),
                Some(a),
                "the best person is still reported"
            );
            None
        }
    };
    assert_eq!(known(0.0), Some(a));
    assert_eq!(known(0.2), Some(a));
    assert_eq!(known(0.3), None);
    assert_eq!(
        match_embedding_with_margin(&w.repo, &near_a, 0.5, 0.0).unwrap(),
        match_embedding_with_report(&w.repo, &near_a, 0.5).unwrap()
    );
    // Alone: nobody to be clear of.
    let mut alone = World::new();
    let c = alone.person("Cy");
    alone.enroll(c, &[emb(&[1.0, 0.0, 0.0])], 12).unwrap();
    assert!(matches!(
        match_embedding_with_margin(&alone.repo, &near_a, 0.5, 0.9).unwrap().outcome,
        MatchOutcome::Known { person, .. } if person == c
    ));
}

#[test]
fn voiceprint_margin_is_chosen_per_model() {
    assert_eq!(
        calibrated_margin(&EmbeddingModelId::new("campplus-en-voxceleb@357a834f702b")),
        Some(CAMPPLUS_MEETING_MARGIN)
    );
    assert_eq!(CAMPPLUS_MEETING_MARGIN, 0.08);
    assert_eq!(calibrated_margin(&model()), None);
}
