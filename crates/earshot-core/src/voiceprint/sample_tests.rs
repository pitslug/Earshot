//! Voice sample and effective print tests (`cargo test -p earshot-core voiceprint`).

use std::time::Duration;

use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::sample::fold;
use super::*;
use crate::identity::{
    ClassMarker, DeviceId, HlcTimestamp, ManualClock, SyncClassKind, sync_envelope,
};

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("test-model@1")
}

fn node() -> DeviceId {
    DeviceId::from_bytes([7; 16])
}

fn meta_at(wall_ms: u64, logical: u32) -> SyncMeta {
    let at = |wall_ms, logical| HlcTimestamp {
        wall_ms,
        logical,
        node: node(),
    };
    SyncMeta {
        created_at: at(wall_ms, logical),
        updated_at: at(wall_ms, logical),
        deleted: false,
        origin_device_id: node(),
    }
}

fn print(base: &[f32], count: u32, speech_ms: u64) -> Voiceprint {
    Voiceprint::from_stored(
        EntityId::from_bytes([1; 16]),
        meta_at(1_800_000_000_000, 0),
        EntityId::from_bytes([2; 16]),
        model(),
        base.to_vec(),
        count,
        speech_ms,
    )
}

fn unit_of(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
    v.iter().map(|&x| (f64::from(x) / n) as f32).collect()
}

/// A live sample record of `print`, created at `wall_ms`.
fn record(id: u8, wall_ms: u64, p: &Voiceprint, vector: Vec<f32>, speech_ms: u64) -> SampleRecord {
    SampleRecord {
        id: EntityId::from_bytes([id; 16]),
        meta: meta_at(wall_ms, 0),
        voiceprint_id: Some(p.id()),
        person_id: Some(p.person_id()),
        model: Some(p.model().clone()),
        vector,
        speech_ms: Some(speech_ms),
        meeting_id: Some(EntityId::from_bytes([30; 16])),
        speaker_id: Some(EntityId::from_bytes([31; 16])),
    }
}

fn sample(id: u8, wall_ms: u64, p: &Voiceprint, v: &[f32], speech_ms: u64) -> VoiceprintSample {
    VoiceprintSample::from_stored(record(id, wall_ms, p, unit_of(v), speech_ms)).unwrap()
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

#[test]
fn voiceprint_wire_json_is_unchanged_by_private_fields() {
    let node = node();
    let at = |wall_ms, logical| HlcTimestamp {
        wall_ms,
        logical,
        node,
    };
    let v = Voiceprint::from_stored(
        EntityId::from_bytes([1; 16]),
        SyncMeta {
            created_at: at(1_800_000_000_000, 0),
            updated_at: at(1_800_000_000_500, 2),
            deleted: false,
            origin_device_id: node,
        },
        EntityId::from_bytes([2; 16]),
        model(),
        vec![0.1, -0.25, 1.0e-7, 3.0],
        3,
        12_345,
    );
    // Captured from the commit before the fields became private.
    let expected = r#"{"id":"01010101-0101-0101-0101-010101010101","meta":{"created_at":{"wall_ms":1800000000000,"logical":0,"node":"07070707-0707-0707-0707-070707070707"},"updated_at":{"wall_ms":1800000000500,"logical":2,"node":"07070707-0707-0707-0707-070707070707"},"deleted":false,"origin_device_id":"07070707-0707-0707-0707-070707070707"},"person_id":"02020202-0202-0202-0202-020202020202","model":"test-model@1","sum":[0.1,-0.25,1e-7,3.0],"count":3,"speech_ms":12345}"#;
    assert_eq!(serde_json::to_string(&v).unwrap(), expected);
    let back: Voiceprint = serde_json::from_str(expected).unwrap();
    assert_eq!(back, v);
}

#[test]
fn voiceprint_sample_is_opt_in_and_goes_through_the_envelope() {
    fn kind<T: SyncClass>() -> SyncClassKind {
        <T::Class as ClassMarker>::KIND
    }
    assert_eq!(kind::<VoiceprintSample>(), SyncClassKind::SyncableOptIn);
    let p = print(&[1.0, 0.0, 0.0], 1, 1000);
    let s = sample(9, 1, &p, &[0.0, 1.0, 0.0], 500);
    let json = serde_json::to_value(sync_envelope(&s)).unwrap();
    assert_eq!(json["entity"], "voiceprint_sample");
}

#[test]
fn voiceprint_sample_check_holds_each_boundary() {
    let ok = |r: SampleRecord| VoiceprintSample::from_stored(r).is_ok();
    let p = print(&[0.0; 4], 0, 0);
    let base = |vector: Vec<f32>| record(5, 1, &p, vector, 1000);

    // Dimension 0 and above the limit refused; 1 and the limit accepted.
    assert!(!ok(base(Vec::new())));
    assert!(ok(base(vec![1.0])));
    let wide = |n: usize| vec![(1.0 / (n as f64).sqrt()) as f32; n];
    assert!(ok(base(wide(MAX_EMBEDDING_DIM))));
    assert!(!ok(base(wide(MAX_EMBEDDING_DIM + 1))));

    // Norm within the tolerance accepted, beyond it refused.
    assert!(ok(base(vec![1.0 + SAMPLE_NORM_TOLERANCE * 0.9])));
    assert!(ok(base(vec![1.0 - SAMPLE_NORM_TOLERANCE * 0.9])));
    assert!(!ok(base(vec![1.0 + SAMPLE_NORM_TOLERANCE * 1.1])));
    assert!(!ok(base(vec![1.0 - SAMPLE_NORM_TOLERANCE * 1.1])));
    assert!(!ok(base(vec![f32::NAN])));
    assert!(!ok(base(vec![f32::INFINITY])));

    // Speech at the limit accepted, above it refused.
    let mut r = base(vec![1.0]);
    r.speech_ms = Some(MAX_SAMPLE_SPEECH_MS);
    assert!(ok(r.clone()));
    r.speech_ms = Some(MAX_SAMPLE_SPEECH_MS + 1);
    assert!(!ok(r));

    // Meeting and speaker together, or neither.
    let mut r = base(vec![1.0]);
    r.speaker_id = None;
    assert!(!ok(r.clone()));
    r.meeting_id = None;
    assert!(ok(r.clone()));
    r.speaker_id = Some(EntityId::from_bytes([31; 16]));
    assert!(!ok(r));

    // A live record without its print, person, model or speech is refused.
    for blank in 0..4 {
        let mut r = base(vec![1.0]);
        match blank {
            0 => r.voiceprint_id = None,
            1 => r.person_id = None,
            2 => r.model = None,
            _ => r.speech_ms = None,
        }
        assert!(!ok(r), "field {blank}");
    }
}

#[test]
fn voiceprint_sample_tombstone_keeps_no_body_whatever_its_record_holds() {
    let p = print(&[0.0; 2], 0, 0);
    let mut r = record(5, 1, &p, vec![f32::NAN, 7.0], u64::MAX);
    r.meta.deleted = true;
    let s = VoiceprintSample::from_stored(r).unwrap();
    assert!(s.is_erased());
    assert_eq!(
        (s.voiceprint_id(), s.person_id(), s.vector(), s.speech_ms()),
        (None, None, None, None)
    );
    let back = SampleRecord::from(s.clone());
    assert!(back.vector.is_empty() && back.model.is_none() && back.meeting_id.is_none());

    let mut meta = meta_at(5, 0);
    assert!(VoiceprintSample::erased(EntityId::from_bytes([5; 16]), meta).is_err());
    meta.deleted = true;
    let e = VoiceprintSample::erased(EntityId::from_bytes([5; 16]), meta).unwrap();
    assert!(e.is_erased());
}

#[test]
fn voiceprint_sample_debug_never_prints_the_vector() {
    let p = print(&[1.0, 0.0], 1, 1);
    let s = sample(9, 1, &p, &[0.123_456_7, 0.987_654_3], 1);
    let text = format!("{s:?}");
    assert!(text.contains("dim: 2"), "{text}");
    for x in s.vector().unwrap() {
        assert!(!text.contains(&x.to_string()), "{text}");
    }
    assert!(!text.contains("vector"), "{text}");

    // Nor the record the sample is stored and sent as.
    let r = SampleRecord::from(s.clone());
    let text = format!("{r:?}");
    assert!(
        text.contains("dim: 2") && text.contains("speech_ms"),
        "{text}"
    );
    for x in s.vector().unwrap() {
        assert!(!text.contains(&x.to_string()), "{text}");
    }
    assert!(!text.contains("vector"), "{text}");
}

#[test]
fn voiceprint_sample_vector_survives_the_wire_bit_for_bit() {
    let p = print(&[0.0; 3], 0, 0);
    // Values that need every bit of an f32.
    let v = [0.1f32.next_up(), -(1.0f32 / 3.0), 2.0f32.sqrt().next_down()];
    let s = sample(9, 1, &p, &v, 1);
    let json = serde_json::to_string(&s).unwrap();
    let back: VoiceprintSample = serde_json::from_str(&json).unwrap();
    assert_eq!(bits(back.vector().unwrap()), bits(s.vector().unwrap()));
    assert_eq!(back, s);
    // A live record that fails the check does not decode.
    let bad = json.replace("\"speech_ms\":1", "\"speech_ms\":null");
    assert!(serde_json::from_str::<VoiceprintSample>(&bad).is_err());
}

#[test]
fn voiceprint_sample_of_the_largest_dimension_fits_the_sync_plaintext() {
    // earshot_sync::MAX_PLAINTEXT (192 KiB), which core cannot name; the real limit is pinned
    // through `seal_row` in earshot-sync (`sync_voice_sample_of_the_largest_dimension_...`).
    const MAX_PLAINTEXT: usize = 192 * 1024;
    let x = -(1.0f32 / 64.0).next_down();
    let p = print(&vec![0.0; MAX_EMBEDDING_DIM], 0, 0);
    let s = VoiceprintSample::from_stored(record(
        9,
        1,
        &p,
        vec![x; MAX_EMBEDDING_DIM],
        MAX_SAMPLE_SPEECH_MS,
    ))
    .unwrap();
    let json = serde_json::to_string(&sync_envelope(&s)).unwrap();
    assert!(json.len() < MAX_PLAINTEXT, "{} bytes", json.len());
}

#[test]
fn voiceprint_sample_new_makes_unit_vectors_and_clamps_speech() {
    let clock = ManualClock::new(1_800_000_000_000);
    let mut rng = ChaCha8Rng::seed_from_u64(1);
    let mut hlc = Hlc::new(DeviceId::generate(&clock, &mut rng));
    let p = print(&[0.0; 2], 0, 0);
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let s =
        VoiceprintSample::new(&p, &[3.0, 4.0], u64::MAX, SampleOrigin::Own, &mut stamp).unwrap();
    assert_eq!(s.vector().unwrap(), &[0.6, 0.8]);
    assert_eq!(s.speech_ms(), Some(MAX_SAMPLE_SPEECH_MS));
    assert_eq!(s.origin(), Some(SampleOrigin::Own));
    assert_eq!(
        (s.voiceprint_id(), s.person_id()),
        (Some(p.id()), Some(p.person_id()))
    );
    let origin = SampleOrigin::Meeting {
        meeting: EntityId::from_bytes([3; 16]),
        speaker: EntityId::from_bytes([4; 16]),
    };
    let m = VoiceprintSample::new(&p, &[1.0, 0.0], 1, origin, &mut stamp).unwrap();
    assert_eq!(m.origin(), Some(origin));
    assert!(VoiceprintSample::new(&p, &[1.0], 1, origin, &mut stamp).is_err());
    assert!(VoiceprintSample::new(&p, &[0.0, 0.0], 1, origin, &mut stamp).is_err());
}

#[test]
fn voiceprint_effective_of_no_samples_is_the_stored_base_bit_for_bit() {
    let p = print(&[0.1, -0.25, 1.0e-7], 3, 12_345);
    let e = effective(&p, &[]);
    assert_eq!(bits(e.sum()), bits(p.base_sum()));
    assert_eq!((e.count(), e.speech_ms(), e.skipped()), (3, 12_345, 0));
    assert!(e.has_base());
    assert!(!effective(&print(&[0.0; 3], 0, 0), &[]).has_base());
}

#[test]
fn voiceprint_effective_adds_live_samples_in_creation_order_whatever_the_input_order() {
    let p = print(&[0.5, 0.25, -0.125], 1, 1000);
    let a = sample(10, 3, &p, &[0.3, 0.1, 0.7], 100);
    let b = sample(11, 1, &p, &[0.9, -0.2, 0.05], 200);
    let c = sample(12, 2, &p, &[-0.4, 0.6, 0.33], 300);
    let forward = effective(&p, &[a.clone(), b.clone(), c.clone()]);
    let backward = effective(&p, &[c.clone(), a.clone(), b.clone()]);
    assert_eq!(bits(forward.sum()), bits(backward.sum()));
    // The order is b, c, a (by created_at): the same f32 additions done by hand.
    let mut want = p.base_sum().to_vec();
    for s in [&b, &c, &a] {
        for (w, x) in want.iter_mut().zip(s.vector().unwrap()) {
            *w += x;
        }
    }
    assert_eq!(bits(forward.sum()), bits(&want));
    assert_eq!((forward.count(), forward.speech_ms()), (4, 1600));
    assert!(forward.has_base());

    // Tombstones and samples of other prints are ignored, not counted.
    let mut erased = meta_at(4, 0);
    erased.deleted = true;
    let gone = VoiceprintSample::erased(EntityId::from_bytes([13; 16]), erased).unwrap();
    let mut r = record(14, 5, &p, unit_of(&[1.0, 0.0, 0.0]), 50);
    r.voiceprint_id = Some(EntityId::from_bytes([99; 16]));
    let other = VoiceprintSample::from_stored(r).unwrap();
    let e = effective(&p, &[gone, other, a]);
    assert_eq!((e.count(), e.skipped()), (2, 0));
}

#[test]
fn voiceprint_effective_skips_samples_it_cannot_add_and_counts_them() {
    let p = print(&[1.0, 0.0], 1, 1000);
    let mut wrong_model = record(10, 1, &p, unit_of(&[0.0, 1.0]), 1);
    wrong_model.model = Some(EmbeddingModelId::new("other@2"));
    let wrong_dim = record(11, 2, &p, unit_of(&[0.0, 1.0, 1.0]), 1);
    let mut wrong_person = record(12, 3, &p, unit_of(&[0.0, 1.0]), 1);
    wrong_person.person_id = Some(EntityId::from_bytes([77; 16]));
    let mut samples: Vec<VoiceprintSample> = [wrong_model, wrong_dim, wrong_person]
        .into_iter()
        .map(|r| VoiceprintSample::from_stored(r).unwrap())
        .collect();
    // A non-finite vector cannot pass the check; the fold still survives one.
    let nan = record(13, 4, &p, vec![f32::NAN, 1.0], 1);
    assert!(VoiceprintSample::from_stored(nan.clone()).is_err());
    samples.push(VoiceprintSample::unchecked_for_test(nan));
    let e = effective(&p, &samples);
    assert_eq!(e.skipped(), 4);
    assert_eq!((e.count(), e.speech_ms()), (1, 1000));
    assert_eq!(bits(e.sum()), bits(p.base_sum()));
}

#[test]
fn voiceprint_effective_count_and_speech_saturate() {
    let p = print(&[1.0, 0.0], u32::MAX, u64::MAX - 1);
    let s = sample(10, 1, &p, &[0.0, 1.0], 500);
    let e = effective(&p, &[s]);
    assert_eq!((e.count(), e.speech_ms()), (u32::MAX, u64::MAX));
}

#[test]
fn voiceprint_fold_leaves_out_excluded_samples() {
    let p = print(&[0.0; 2], 0, 0);
    let a = sample(10, 1, &p, &[1.0, 0.0], 100);
    let b = sample(11, 2, &p, &[0.0, 1.0], 200);
    let e = fold(&p, &[a.clone(), b.clone()], &[a.id()]);
    assert_eq!((e.count(), e.speech_ms()), (1, 200));
    assert_eq!(bits(e.sum()), bits(effective(&p, &[b]).sum()));
    assert!(!e.has_base());
}

fn world() -> (ManualClock, Hlc, ChaCha8Rng, InMemoryVoiceprintRepo) {
    let clock = ManualClock::new(1_800_000_000_000);
    let mut rng = ChaCha8Rng::seed_from_u64(7);
    let hlc = Hlc::new(DeviceId::generate(&clock, &mut rng));
    (clock, hlc, rng, InMemoryVoiceprintRepo::new())
}

#[test]
fn voiceprint_an_empty_print_is_neither_scored_nor_counted_as_skipped() {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let ada = Person::new("Ada", false, &mut stamp);
    repo.put_person(ada.clone()).unwrap();
    let (id, meta) = stamp.new_row();
    // A valid direction but count 0: it holds no voice.
    repo.put_voiceprint(Voiceprint::from_stored(
        id,
        meta,
        ada.id,
        model(),
        vec![1.0, 0.0],
        0,
        0,
    ))
    .unwrap();
    let e = Embedding::new(model(), vec![1.0, 0.0]);
    let r = match_embedding_with_report(&repo, &e, 0.0).unwrap();
    assert_eq!(r.outcome, MatchOutcome::Unknown { best: None });
    assert_eq!(r.skipped_invalid, 0);

    // A print with a count and an invalid centroid is still counted as skipped.
    let bob = Person::new("Bob", false, &mut stamp);
    repo.put_person(bob.clone()).unwrap();
    let (id, meta) = stamp.new_row();
    repo.put_voiceprint(Voiceprint::from_stored(
        id,
        meta,
        bob.id,
        model(),
        vec![0.0, 0.0],
        2,
        0,
    ))
    .unwrap();
    let r = match_embedding_with_report(&repo, &e, 0.0).unwrap();
    assert_eq!(r.skipped_invalid, 1);
}

#[test]
fn voiceprint_an_empty_print_needs_new_speech_and_admissible_writes_nothing() {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let ada = Person::new("Ada", false, &mut stamp);
    repo.put_person(ada.clone()).unwrap();
    let (id, meta) = stamp.new_row();
    repo.put_voiceprint(Voiceprint::from_stored(
        id,
        meta,
        ada.id,
        model(),
        vec![0.0, 0.0],
        0,
        0,
    ))
    .unwrap();
    let config = EnrolmentConfig::default();
    let e = [Embedding::new(model(), vec![0.0, 2.0])];
    let short = Duration::from_secs(3);
    let refused = Err(VoiceprintError::NotEnoughSpeech {
        needed: config.min_new_speech,
        got: short,
    });
    assert_eq!(admissible(&repo, ada.id, &e, short, &config, &[]), refused);
    assert_eq!(
        enroll(
            &mut repo,
            ada.id,
            &e,
            short,
            SampleOrigin::Own,
            &config,
            &mut stamp
        )
        .map(|_| ()),
        refused
    );
    let before = repo.voiceprints_of(ada.id).unwrap();
    assert_eq!(before[0].base_count(), 0, "a refusal writes nothing");

    // With enough speech it adds into the empty print, which then scores.
    let long = Duration::from_secs(12);
    assert_eq!(admissible(&repo, ada.id, &e, long, &config, &[]), Ok(()));
    assert_eq!(
        enroll(
            &mut repo,
            ada.id,
            &e,
            long,
            SampleOrigin::Own,
            &config,
            &mut stamp
        ),
        Ok(id)
    );
    let (_, eff) = live_prints_of(&repo, ada.id).unwrap().pop().unwrap();
    assert_eq!((eff.count(), eff.speech_ms()), (1, 12_000));
    let r = match_embedding(&repo, &e[0], 0.5).unwrap();
    assert!(matches!(r, MatchOutcome::Known { person, .. } if person == ada.id));

    // admissible refuses what enroll refuses: an unknown person, another dimension.
    let nobody = EntityId::from_bytes([200; 16]);
    assert_eq!(
        admissible(&repo, nobody, &e, long, &config, &[]),
        Err(VoiceprintError::UnknownPerson(nobody))
    );
    let wide = [Embedding::new(model(), vec![1.0, 0.0, 0.0])];
    assert!(admissible(&repo, ada.id, &wide, long, &config, &[]).is_err());
    assert_eq!(
        admissible(&repo, ada.id, &[], long, &config, &[]),
        Err(VoiceprintError::NoEmbeddings)
    );
}

#[test]
fn voiceprint_live_prints_leave_out_tombstones_and_other_models() {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let ada = Person::new("Ada", false, &mut stamp);
    repo.put_person(ada.clone()).unwrap();
    let mut put = |m: &str, deleted: bool, stamp: &mut Stamp<'_, ManualClock, ChaCha8Rng>| {
        let (id, mut meta) = stamp.new_row();
        meta.deleted = deleted;
        repo.put_voiceprint(Voiceprint::from_stored(
            id,
            meta,
            ada.id,
            EmbeddingModelId::new(m),
            vec![1.0, 0.0],
            1,
            10,
        ))
        .unwrap();
        id
    };
    let live = put("test-model@1", false, &mut stamp);
    put("test-model@1", true, &mut stamp);
    let other = put("other@2", false, &mut stamp);
    let ids = |v: Vec<(Voiceprint, Effective)>| v.iter().map(|(p, _)| p.id()).collect::<Vec<_>>();
    assert_eq!(ids(live_prints(&repo, &model()).unwrap()), vec![live]);
    let mut of = ids(live_prints_of(&repo, ada.id).unwrap());
    of.sort();
    let mut want = vec![live, other];
    want.sort();
    assert_eq!(of, want);
}

#[test]
fn voiceprint_in_memory_repo_passes_the_sample_contract() {
    crate::model::repo_contract::voiceprint_sample_repo(InMemoryVoiceprintRepo::new);
}

#[test]
fn voiceprint_a_zero_base_print_with_stored_samples_reads_as_a_voice() {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let me = Person::new(DEFAULT_ME_NAME, true, &mut stamp);
    repo.put_person(me.clone()).unwrap();
    let (id, meta) = stamp.new_row();
    let print = Voiceprint::from_stored(id, meta, me.id, model(), vec![0.0, 0.0], 0, 0);
    repo.put_voiceprint(print.clone()).unwrap();
    let e = Embedding::new(model(), vec![0.0, 1.0]);
    // Empty: no voice, needs new speech.
    assert_eq!(
        match_embedding(&repo, &e, 0.5).unwrap(),
        MatchOutcome::Unknown { best: None }
    );
    let short = Duration::from_secs(1);
    let config = EnrolmentConfig::default();
    assert!(admissible(&repo, me.id, std::slice::from_ref(&e), short, &config, &[]).is_err());

    let s =
        VoiceprintSample::new(&print, &[0.0, 3.0], 7_000, SampleOrigin::Own, &mut stamp).unwrap();
    repo.put_sample(s.clone()).unwrap();
    let (_, eff) = live_prints_of(&repo, me.id).unwrap().pop().unwrap();
    assert_eq!(
        (eff.count(), eff.speech_ms(), eff.has_base()),
        (1, 7_000, false)
    );
    let (_, by_model) = live_prints(&repo, &model()).unwrap().pop().unwrap();
    assert_eq!(by_model, eff);
    assert!(matches!(
        match_embedding(&repo, &e, 0.5).unwrap(),
        MatchOutcome::Known { person, .. } if person == me.id
    ));
    // It holds a voice now, so an update needs no new speech; leaving its own sample out
    // judges it empty again.
    assert_eq!(
        admissible(&repo, me.id, std::slice::from_ref(&e), short, &config, &[]),
        Ok(())
    );
    assert!(
        admissible(
            &repo,
            me.id,
            std::slice::from_ref(&e),
            short,
            &config,
            &[s.id()]
        )
        .is_err()
    );
    assert!(
        enroll_me(
            &mut repo,
            &[e],
            short,
            SampleOrigin::Own,
            &config,
            &mut stamp
        )
        .is_ok()
    );
}

/// Fixed enrolment inputs: two calls of two and one embeddings, values that round in f32.
fn literal_inputs() -> [(Vec<Embedding>, Duration); 2] {
    let e = |v: [f32; 3]| Embedding::new(model(), v.to_vec());
    [
        (
            vec![e([0.3, 0.7, -0.2]), e([0.11, -0.53, 0.97])],
            Duration::from_millis(12_345),
        ),
        (vec![e([-0.61, 0.29, 0.13])], Duration::from_millis(4_001)),
    ]
}

/// A repo with one live person and what `enrol` did to it with [`literal_inputs`].
fn enrolled_with(
    enrol: impl Fn(
        &mut InMemoryVoiceprintRepo,
        EntityId,
        &[Embedding],
        Duration,
        &mut Stamp<'_, ManualClock, ChaCha8Rng>,
    ) -> EntityId,
) -> (InMemoryVoiceprintRepo, EntityId, Effective) {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let ada = Person::new("Ada", false, &mut stamp);
    repo.put_person(ada.clone()).unwrap();
    let mut print = None;
    for (e, speech) in literal_inputs() {
        clock.advance_ms(1000);
        print = Some(enrol(&mut repo, ada.id, &e, speech, &mut stamp));
    }
    let print = print.unwrap();
    let (_, eff) = live_prints_of(&repo, ada.id)
        .unwrap()
        .into_iter()
        .find(|(v, _)| v.id() == print)
        .unwrap();
    (repo, ada.id, eff)
}

fn by_enroll(
    repo: &mut InMemoryVoiceprintRepo,
    p: EntityId,
    e: &[Embedding],
    s: Duration,
    st: &mut Stamp<'_, ManualClock, ChaCha8Rng>,
) -> EntityId {
    enroll(
        repo,
        p,
        e,
        s,
        SampleOrigin::Own,
        &EnrolmentConfig::default(),
        st,
    )
    .unwrap()
}

/// Bits of the sum, count and speech today's `enroll` (`Voiceprint::add`) gives for
/// [`literal_inputs`], captured on the commit before samples were written.
const LITERAL_SUM_BITS: [u32; 3] = [3_201_321_966, 1_062_559_678, 1_062_135_987];

#[test]
fn voiceprint_enroll_gives_the_literal_sums_captured_before_samples() {
    let (_, _, eff) = enrolled_with(by_enroll);
    assert_eq!(bits(eff.sum()), LITERAL_SUM_BITS);
    assert_eq!((eff.count(), eff.speech_ms()), (3, 16_346));
}

#[test]
fn voiceprint_enroll_writes_one_zero_base_print_and_a_sample_per_embedding() {
    let (repo, ada, got) = enrolled_with(by_enroll);
    assert_eq!(bits(got.sum()), LITERAL_SUM_BITS);
    assert_eq!((got.count(), got.speech_ms()), (3, 16_346));
    // A zero-base print row, written once, and one sample per embedding.
    let prints = repo.voiceprints_of(ada).unwrap();
    assert_eq!(prints.len(), 1);
    assert!(!got.has_base());
    let samples = repo.samples_of(prints[0].id()).unwrap();
    assert_eq!(samples.len(), 3);
    // The speech split sums exactly: 12_345 over two (the first takes the remainder), then
    // 4_001.
    let speech: Vec<u64> = samples.iter().map(|s| s.speech_ms().unwrap()).collect();
    assert_eq!(speech, vec![6_173, 6_172, 4_001]);
    // Input order is creation order.
    let stamps: Vec<_> = samples.iter().map(|s| s.meta().created_at).collect();
    assert!(stamps.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn voiceprint_enroll_adds_to_the_earliest_of_two_live_prints_and_checks_first() {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let ada = Person::new("Ada", false, &mut stamp);
    repo.put_person(ada.clone()).unwrap();
    let config = EnrolmentConfig::default();
    let e = [Embedding::new(model(), vec![1.0, 0.0])];
    let own = SampleOrigin::Own;
    // Too little speech for a first print: refused, nothing written.
    let short = Duration::from_secs(2);
    assert!(matches!(
        enroll(&mut repo, ada.id, &e, short, own, &config, &mut stamp),
        Err(VoiceprintError::NotEnoughSpeech { .. })
    ));
    assert!(repo.voiceprints_of(ada.id).unwrap().is_empty());
    assert!(repo.samples_of_person(ada.id).unwrap().is_empty());
    // Two live prints (two computers each made one): the earlier-created gets the sample,
    // even with the larger id.
    let (later_id, later_meta) = stamp.new_row();
    clock.advance_ms(1);
    let (_, mut early_meta) = stamp.new_row();
    early_meta.created_at.wall_ms = later_meta.created_at.wall_ms - 10;
    let early_id = EntityId::from_bytes([0xff; 16]);
    for (id, meta) in [(later_id, later_meta), (early_id, early_meta)] {
        let v = Voiceprint::from_stored(id, meta, ada.id, model(), vec![0.5, 0.5], 1, 10_000);
        repo.put_voiceprint(v).unwrap();
    }
    let one = Duration::from_secs(1);
    let got = enroll(&mut repo, ada.id, &e, one, own, &config, &mut stamp).unwrap();
    assert_eq!(got, early_id);
    assert_eq!(repo.samples_of(early_id).unwrap().len(), 1);
    assert!(repo.samples_of(later_id).unwrap().is_empty());
}

/// The person "Ada" with a based print, then samples from two meeting voices and her own.
fn ada_with_samples() -> (
    ManualClock,
    Hlc,
    InMemoryVoiceprintRepo,
    EntityId,
    Voiceprint,
    [SampleOrigin; 2],
) {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let ada = Person::new("Ada", false, &mut stamp);
    repo.put_person(ada.clone()).unwrap();
    let (id, meta) = stamp.new_row();
    let print = Voiceprint::from_stored(id, meta, ada.id, model(), vec![0.6, 0.8], 1, 10_000);
    repo.put_voiceprint(print.clone()).unwrap();
    let origins = [
        SampleOrigin::Meeting {
            meeting: EntityId::from_bytes([0x41; 16]),
            speaker: EntityId::from_bytes([0x42; 16]),
        },
        SampleOrigin::Meeting {
            meeting: EntityId::from_bytes([0x41; 16]),
            speaker: EntityId::from_bytes([0x43; 16]),
        },
    ];
    let config = EnrolmentConfig::default();
    let e = |v: [f32; 2]| [Embedding::new(model(), v.to_vec())];
    let short = Duration::from_secs(1);
    for (v, origin) in [
        ([0.1, 0.9], origins[0]),
        ([0.9, -0.3], origins[1]),
        ([0.2, 0.2], SampleOrigin::Own),
    ] {
        enroll(&mut repo, ada.id, &e(v), short, origin, &config, &mut stamp).unwrap();
    }
    (clock, hlc, repo, ada.id, print, origins)
}

fn meeting_voice(o: SampleOrigin) -> (EntityId, EntityId) {
    match o {
        SampleOrigin::Meeting { meeting, speaker } => (meeting, speaker),
        SampleOrigin::Own => unreachable!("a meeting voice"),
    }
}

#[test]
fn voiceprint_take_back_restores_the_print_exactly_and_respects_keep() {
    let (clock, mut hlc, mut repo, ada, print, origins) = ada_with_samples();
    let (meeting, speaker) = meeting_voice(origins[0]);
    let all = repo.samples_of(print.id()).unwrap();
    let rest: Vec<VoiceprintSample> = all
        .iter()
        .filter(|s| s.origin() != Some(origins[0]))
        .cloned()
        .collect();
    // Kept: the voice still resolves to Ada.
    let n = take_back(&mut repo, meeting, speaker, Some(ada), &mut hlc, &clock).unwrap();
    assert_eq!(n, 0);
    // Taken back: exactly that sample, as if it had never been added.
    let n = take_back(&mut repo, meeting, speaker, None, &mut hlc, &clock).unwrap();
    assert_eq!(n, 1);
    let (_, now) = live_prints_of(&repo, ada).unwrap().pop().unwrap();
    assert_eq!(now, effective(&print, &rest));
    assert_eq!(bits(now.sum()), bits(effective(&print, &rest).sum()));
    assert_eq!(repo.samples_of(print.id()).unwrap().len(), 2);
    // Its tombstone holds nothing but its id and metadata.
    let gone = all.iter().find(|s| s.origin() == Some(origins[0])).unwrap();
    let stored = repo.sample(gone.id()).unwrap().unwrap();
    assert!(stored.is_erased() && stored.meta().deleted);
    assert!(stored.meta().updated_at > gone.meta().updated_at);
    // Nothing left from that voice: a second take back writes nothing.
    let n = take_back(&mut repo, meeting, speaker, None, &mut hlc, &clock).unwrap();
    assert_eq!(n, 0);
    // Keeping another person takes the voice back from Ada.
    let (meeting, speaker) = meeting_voice(origins[1]);
    let other = Some(EntityId::from_bytes([0x77; 16]));
    let n = take_back(&mut repo, meeting, speaker, other, &mut hlc, &clock).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn voiceprint_forget_erases_samples_by_person_and_by_print() {
    let (clock, mut hlc, mut repo, ada, print, _) = ada_with_samples();
    let meta = {
        let mut rng = ChaCha8Rng::seed_from_u64(9);
        let mut st = Stamp {
            clock: &clock,
            hlc: &mut hlc,
            rng: &mut rng,
        };
        st.new_row().1
    };
    let at = meta.created_at.wall_ms;
    // A sample of the print written for another person id (a merged duplicate's row), and one
    // of hers whose print this repo does not hold: both are hers to erase.
    let mut by_print = record(0x51, at, &print, unit_of(&[1.0, 1.0]), 5);
    by_print.person_id = Some(EntityId::from_bytes([0x99; 16]));
    repo.put_sample(VoiceprintSample::from_stored(by_print).unwrap())
        .unwrap();
    let missing_print = Voiceprint::from_stored(
        EntityId::from_bytes([0x60; 16]),
        meta,
        ada,
        model(),
        vec![0.0, 0.0],
        0,
        0,
    );
    let orphan = record(0x52, at, &missing_print, unit_of(&[1.0, 0.0]), 5);
    repo.put_sample(VoiceprintSample::from_stored(orphan).unwrap())
        .unwrap();

    let n = forget_voiceprints(&mut repo, ada, &mut hlc, &clock).unwrap();
    assert_eq!(
        n,
        Forgotten {
            voiceprints: 1,
            samples: 5
        }
    );
    assert_eq!(n.total(), 6);
    assert!(repo.samples_of(print.id()).unwrap().is_empty());
    assert!(repo.samples_of_person(ada).unwrap().is_empty());
    assert!(repo.samples_of(missing_print.id()).unwrap().is_empty());
    for id in [0x51u8, 0x52] {
        let s = repo
            .sample(EntityId::from_bytes([id; 16]))
            .unwrap()
            .unwrap();
        assert!(s.is_erased());
    }
    let again = forget_voiceprints(&mut repo, ada, &mut hlc, &clock).unwrap();
    assert_eq!(again, Forgotten::default());
}

#[test]
fn voiceprint_saved_before_samples_needs_the_marker_no_sample_and_a_base() {
    let (clock, mut hlc, mut repo, ada, _, origins) = ada_with_samples();
    let (meeting, saved_voice) = meeting_voice(origins[0]);
    let fresh = EntityId::from_bytes([0x44; 16]);
    let m = model();
    // Marker, no sample from the voice, a print with a base: saved before samples.
    assert!(saved_before_samples(&repo, true, meeting, fresh, ada, &m).unwrap());
    assert!(!saved_before_samples(&repo, false, meeting, fresh, ada, &m).unwrap());
    // A live sample from the voice: saved as a sample, not before.
    assert!(!saved_before_samples(&repo, true, meeting, saved_voice, ada, &m).unwrap());
    // Forgotten elsewhere and a new zero-base print made: the marker alone no longer counts.
    forget_voiceprints(&mut repo, ada, &mut hlc, &clock).unwrap();
    let meta = {
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        let mut st = Stamp {
            clock: &clock,
            hlc: &mut hlc,
            rng: &mut rng,
        };
        st.new_row().1
    };
    let id = EntityId::from_bytes([0x61; 16]);
    let new = Voiceprint::from_stored(id, meta, ada, m.clone(), vec![0.0, 0.0], 0, 0);
    repo.put_voiceprint(new).unwrap();
    assert!(!saved_before_samples(&repo, true, meeting, fresh, ada, &m).unwrap());
}

#[test]
fn voiceprint_take_back_resolves_keep_through_merges() {
    let (clock, mut hlc, mut repo, ada, _, origins) = ada_with_samples();
    let (meeting, speaker) = meeting_voice(origins[0]);
    // A duplicate merged into Ada: keeping it keeps Ada's voice.
    let mut rng = ChaCha8Rng::seed_from_u64(12);
    let dup = {
        let mut st = Stamp {
            clock: &clock,
            hlc: &mut hlc,
            rng: &mut rng,
        };
        Person::new("Ada (laptop)", false, &mut st)
    };
    repo.put_person(dup.clone()).unwrap();
    merge_persons(&mut repo, dup.id, ada, &mut hlc, &clock).unwrap();
    let n = take_back(&mut repo, meeting, speaker, Some(dup.id), &mut hlc, &clock).unwrap();
    assert_eq!(n, 0, "the kept person resolves to Ada");
    assert_eq!(repo.samples_from(meeting, speaker).unwrap().len(), 1);
    // A kept person that resolves to nobody keeps nothing.
    let gone = EntityId::from_bytes([0x78; 16]);
    let n = take_back(&mut repo, meeting, speaker, Some(gone), &mut hlc, &clock).unwrap();
    assert_eq!(n, 1);
}

/// T12.77b6: `admissible` and `enroll` judge and write the same print. With two live prints
/// whose id order and creation order disagree (the earlier-created empty, the later-created
/// holding a voice), both take the earliest: too little speech for it is refused and nothing is
/// written, enough speech adds to it.
#[test]
fn voiceprint_admissible_and_enroll_choose_the_same_of_two_prints() {
    let (clock, mut hlc, mut rng, mut repo) = world();
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    let ada = Person::new("Ada", false, &mut stamp);
    repo.put_person(ada.clone()).unwrap();
    let (_, later_meta) = stamp.new_row();
    let mut early_meta = later_meta;
    early_meta.created_at.wall_ms -= 10;
    let (early, later) = (
        EntityId::from_bytes([0xee; 16]),
        EntityId::from_bytes([0x11; 16]),
    );
    let put = |repo: &mut InMemoryVoiceprintRepo, id, meta, sum: Vec<f32>, count| {
        let v = Voiceprint::from_stored(
            id,
            meta,
            ada.id,
            model(),
            sum,
            count,
            10_000 * u64::from(count),
        );
        repo.put_voiceprint(v).unwrap();
    };
    put(&mut repo, early, early_meta, vec![0.0, 0.0], 0);
    put(&mut repo, later, later_meta, vec![0.6, 0.8], 1);
    let config = EnrolmentConfig::default();
    let e = [Embedding::new(model(), vec![1.0, 0.0])];
    let short = Duration::from_secs(1);
    let refused = Err(VoiceprintError::NotEnoughSpeech {
        needed: config.min_new_speech,
        got: short,
    });
    assert_eq!(admissible(&repo, ada.id, &e, short, &config, &[]), refused);
    let wrote = enroll(
        &mut repo,
        ada.id,
        &e,
        short,
        SampleOrigin::Own,
        &config,
        &mut stamp,
    );
    assert_eq!(wrote.map(|_| ()), refused);
    assert!(repo.samples_of_person(ada.id).unwrap().is_empty());
    let long = Duration::from_secs(12);
    let got = enroll(
        &mut repo,
        ada.id,
        &e,
        long,
        SampleOrigin::Own,
        &config,
        &mut stamp,
    )
    .unwrap();
    assert_eq!(got, early);
    assert!(repo.samples_of(later).unwrap().is_empty());
}
