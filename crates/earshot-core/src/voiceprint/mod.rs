//! Voiceprints (PLAN.md 3.5, T3.4): recurring people, their speaker embeddings, enrolment and
//! open-set matching.
//!
//! - [`Person`]: a recurring identity (syncable) with optional `merged_into`, so duplicates
//!   created on two devices can be merged; [`resolve_person`] follows the chain.
//! - [`Voiceprint`]: the running centroid of one person's embeddings from **one** embedding
//!   model (sync class opt-in, D6). Embeddings of other models are never compared with it.
//! - [`VoiceprintRepo`]: storage trait; [`InMemoryVoiceprintRepo`] for tests (SQLite lives in
//!   `earshot-store`).
//! - [`enroll`], [`enroll_me`]: record an enrolment as voice samples of a person's voiceprint
//!   (created with a zero base when there is none).
//! - [`match_embedding`]: open-set identification: the best-scoring person (cosine
//!   similarity to the voiceprint centroid) if the score reaches the threshold, otherwise
//!   "unknown".
//! - [`calibrated_threshold`]: the open-set thresholds fixed on the LibriSpeech dev-clean
//!   split only ([`calibrate_threshold`] is the rule; PLAN.md 4.0, AGENTS.md invariant), per
//!   exact embedding-model id.
//!
//! Running centroid: a voiceprint keeps the sum of its unit-length embeddings and their count;
//! its centroid is the normalised sum, so every enrolment embedding has equal weight and an
//! update is O(dimension). What is scored is the effective print ([`effective`]): the stored
//! base plus the print's live [`VoiceprintSample`]s (docs/voice-samples.md).

mod sample;
#[cfg(test)]
mod sample_tests;
#[cfg(test)]
mod tests;

pub use sample::{
    Effective, MAX_EMBEDDING_DIM, MAX_SAMPLE_SPEECH_MS, SAMPLE_NORM_TOLERANCE, SampleOrigin,
    SampleRecord, VoiceprintSample, admissible, effective, enroll, live_prints, live_prints_of,
    saved_before_samples, take_back, unit_vector,
};

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use rand_core::Rng;
use serde::{Deserialize, Serialize};

use crate::diar::{DiarError, Embedding, EmbeddingModelId};
use crate::identity::{
    Clock, EntityId, Hlc, SyncClass, SyncMeta, SyncSerialize, SyncableClass, SyncableOptInClass,
};

/// Voiceprint errors.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceprintError {
    /// Embedding model mismatch, bad dimension or invalid vector.
    Embedding(DiarError),
    /// The person does not exist (or is deleted).
    UnknownPerson(EntityId),
    /// A new voiceprint needs at least this much speech.
    NotEnoughSpeech {
        /// Required.
        needed: Duration,
        /// Given.
        got: Duration,
    },
    /// No input embeddings.
    NoEmbeddings,
    /// No open-set threshold has been calibrated for this embedding model.
    NoThreshold(EmbeddingModelId),
    /// A voice sample that fails its check (docs/voice-samples.md 2.1).
    InvalidSample(String),
    /// Storage failure.
    Repo(String),
}

impl fmt::Display for VoiceprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VoiceprintError::Embedding(e) => write!(f, "voiceprint embedding: {e}"),
            VoiceprintError::UnknownPerson(id) => write!(f, "unknown person {id:?}"),
            VoiceprintError::NotEnoughSpeech { needed, got } => write!(
                f,
                "enrolment needs {:.1} s of speech, got {:.1} s",
                needed.as_secs_f64(),
                got.as_secs_f64()
            ),
            VoiceprintError::NoEmbeddings => f.write_str("no embeddings to enrol"),
            VoiceprintError::NoThreshold(m) => {
                write!(
                    f,
                    "no open-set threshold calibrated for embedding model {m}"
                )
            }
            VoiceprintError::InvalidSample(m) => write!(f, "voice sample: {m}"),
            VoiceprintError::Repo(m) => write!(f, "voiceprint storage: {m}"),
        }
    }
}

impl std::error::Error for VoiceprintError {}

impl From<DiarError> for VoiceprintError {
    fn from(e: DiarError) -> Self {
        VoiceprintError::Embedding(e)
    }
}

/// Clock, hybrid logical clock and RNG used to stamp new and changed rows.
pub struct Stamp<'a, C: Clock + ?Sized, R: Rng + ?Sized> {
    /// Wall clock.
    pub clock: &'a C,
    /// This device's HLC.
    pub hlc: &'a mut Hlc,
    /// Randomness for new ids.
    pub rng: &'a mut R,
}

impl<C: Clock + ?Sized, R: Rng + ?Sized> Stamp<'_, C, R> {
    fn new_row(&mut self) -> (EntityId, SyncMeta) {
        (
            EntityId::generate(self.clock, self.rng),
            SyncMeta::new(self.hlc, self.clock),
        )
    }
}

/// A recurring identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    /// Row id.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// Display name ("Me" for the user until renamed).
    pub display_name: String,
    /// True for the user of this installation.
    pub is_me: bool,
    /// Set when this person was merged into another; lookups follow it.
    pub merged_into: Option<EntityId>,
}

impl SyncClass for Person {
    type Class = SyncableClass;
}

impl SyncSerialize for Person {
    const ENTITY: &'static str = "person";
    fn sync_id(&self) -> EntityId {
        self.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

impl Person {
    /// A new person.
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        display_name: impl Into<String>,
        is_me: bool,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Self {
        let (id, meta) = stamp.new_row();
        Person {
            id,
            meta,
            display_name: display_name.into(),
            is_me,
            merged_into: None,
        }
    }

    /// Not deleted and not merged away.
    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.meta.deleted && self.merged_into.is_none()
    }
}

/// One person's voiceprint for one embedding model: the running centroid of their
/// enrolment embeddings.
///
/// The stored `sum`, `count` and `speech_ms` are the untracked base (docs/voice-samples.md
/// 3.1): enrolments recorded before voice samples existed. What a print scores with is its
/// effective print, the base plus its live [`VoiceprintSample`]s ([`effective`],
/// [`live_prints`]); only the fold, the store and sync decode read the base (`base_*`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Voiceprint {
    id: EntityId,
    meta: SyncMeta,
    person_id: EntityId,
    // The embedding model; only embeddings of this exact model are compared.
    model: EmbeddingModelId,
    // Sum of the unit-length enrolment embeddings of the base.
    sum: Vec<f32>,
    // Number of embeddings in `sum`.
    count: u32,
    // Speech behind the base embeddings (milliseconds).
    speech_ms: u64,
}

impl SyncClass for Voiceprint {
    type Class = SyncableOptInClass;
}

impl SyncSerialize for Voiceprint {
    const ENTITY: &'static str = "voiceprint";
    fn sync_id(&self) -> EntityId {
        self.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

/// Unit-length copy of `v` (f64 accumulation), or `None` if it is all zeros or not finite.
fn unit(v: &[f32]) -> Option<Vec<f32>> {
    if v.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let n = v
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    if n == 0.0 {
        return None;
    }
    Some(v.iter().map(|&x| (f64::from(x) / n) as f32).collect())
}

impl Voiceprint {
    /// A voiceprint row as stored (the store's row reader and tests); `sum`, `count` and
    /// `speech_ms` are its base.
    #[must_use]
    pub fn from_stored(
        id: EntityId,
        meta: SyncMeta,
        person_id: EntityId,
        model: EmbeddingModelId,
        sum: Vec<f32>,
        count: u32,
        speech_ms: u64,
    ) -> Self {
        Voiceprint {
            id,
            meta,
            person_id,
            model,
            sum,
            count,
            speech_ms,
        }
    }

    /// Row id.
    #[must_use]
    pub fn id(&self) -> EntityId {
        self.id
    }

    /// Sync metadata.
    #[must_use]
    pub fn meta(&self) -> &SyncMeta {
        &self.meta
    }

    /// The person.
    #[must_use]
    pub fn person_id(&self) -> EntityId {
        self.person_id
    }

    /// The embedding model; only embeddings of this exact model are compared.
    #[must_use]
    pub fn model(&self) -> &EmbeddingModelId {
        &self.model
    }

    /// The stored base sum (store and sync only; everything else reads [`effective`]).
    #[must_use]
    pub fn base_sum(&self) -> &[f32] {
        &self.sum
    }

    /// The stored base count (store and sync only).
    #[must_use]
    pub fn base_count(&self) -> u32 {
        self.count
    }

    /// The stored base speech in milliseconds (store and sync only).
    #[must_use]
    pub fn base_speech_ms(&self) -> u64 {
        self.speech_ms
    }
}

/// Unit-length vectors of `embeddings`, all of `model` and (if given) dimension `dim`.
fn check_embeddings(
    embeddings: &[Embedding],
    model: &EmbeddingModelId,
    dim: Option<usize>,
) -> Result<Vec<Vec<f32>>, VoiceprintError> {
    if embeddings.is_empty() {
        return Err(VoiceprintError::NoEmbeddings);
    }
    let dim = dim.unwrap_or(embeddings[0].vector.len());
    embeddings
        .iter()
        .map(|e| {
            if &e.model != model {
                return Err(DiarError::ModelMismatch {
                    left: model.clone(),
                    right: e.model.clone(),
                }
                .into());
            }
            if e.vector.len() != dim {
                return Err(DiarError::Shape(format!(
                    "embedding dimension {} differs from {dim}",
                    e.vector.len()
                ))
                .into());
            }
            unit(&e.vector).ok_or_else(|| {
                DiarError::Shape("embedding is non-finite or all zeros".to_owned()).into()
            })
        })
        .collect()
}

/// Storage of persons and voiceprints. Reads return deleted rows too (tombstones sync);
/// callers filter with [`Person::is_live`] and `meta.deleted`.
pub trait VoiceprintRepo {
    /// The person with `id`.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn person(&self, id: EntityId) -> Result<Option<Person>, VoiceprintError>;
    /// All persons.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn persons(&self) -> Result<Vec<Person>, VoiceprintError>;
    /// Inserts or replaces a person.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn put_person(&mut self, person: Person) -> Result<(), VoiceprintError>;
    /// All voiceprints of `model` (exact id).
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn voiceprints(&self, model: &EmbeddingModelId) -> Result<Vec<Voiceprint>, VoiceprintError>;
    /// All voiceprints of `person`, any model.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn voiceprints_of(&self, person: EntityId) -> Result<Vec<Voiceprint>, VoiceprintError>;
    /// Inserts or replaces a voiceprint.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn put_voiceprint(&mut self, voiceprint: Voiceprint) -> Result<(), VoiceprintError>;
    /// The live voice samples of `model`, in creation order (`meta.created_at`, then id). A
    /// sample tombstone keeps no body, so it matches no key and only its own id finds it
    /// ([`VoiceprintRepo::sample`]); the same holds for every keyed sample read.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn samples(&self, model: &EmbeddingModelId) -> Result<Vec<VoiceprintSample>, VoiceprintError>;
    /// The live samples of the voiceprint `print`, in creation order.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn samples_of(&self, print: EntityId) -> Result<Vec<VoiceprintSample>, VoiceprintError>;
    /// The live samples written for `person` (their `person_id`, not following merges), in
    /// creation order.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn samples_of_person(&self, person: EntityId)
    -> Result<Vec<VoiceprintSample>, VoiceprintError>;
    /// The live samples from the voice `speaker` of `meeting`, in creation order.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn samples_from(
        &self,
        meeting: EntityId,
        speaker: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError>;
    /// The sample with `id`, a tombstone included.
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn sample(&self, id: EntityId) -> Result<Option<VoiceprintSample>, VoiceprintError>;
    /// Inserts or replaces a sample (by id).
    ///
    /// # Errors
    /// [`VoiceprintError::Repo`] on storage failure.
    fn put_sample(&mut self, sample: VoiceprintSample) -> Result<(), VoiceprintError>;
}

/// In-memory [`VoiceprintRepo`] (tests, and the core pipeline before storage exists).
#[derive(Debug, Clone, Default)]
pub struct InMemoryVoiceprintRepo {
    persons: BTreeMap<EntityId, Person>,
    voiceprints: BTreeMap<EntityId, Voiceprint>,
    samples: BTreeMap<EntityId, VoiceprintSample>,
}

impl InMemoryVoiceprintRepo {
    /// The live samples for which `keep` holds, in creation order.
    fn samples_where(&self, keep: impl Fn(&VoiceprintSample) -> bool) -> Vec<VoiceprintSample> {
        let mut out: Vec<VoiceprintSample> = self
            .samples
            .values()
            .filter(|s| !s.is_erased() && keep(s))
            .cloned()
            .collect();
        out.sort_by_key(|s| (s.meta().created_at, s.id()));
        out
    }
}

impl InMemoryVoiceprintRepo {
    /// An empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl VoiceprintRepo for InMemoryVoiceprintRepo {
    fn person(&self, id: EntityId) -> Result<Option<Person>, VoiceprintError> {
        Ok(self.persons.get(&id).cloned())
    }
    fn persons(&self) -> Result<Vec<Person>, VoiceprintError> {
        Ok(self.persons.values().cloned().collect())
    }
    fn put_person(&mut self, person: Person) -> Result<(), VoiceprintError> {
        self.persons.insert(person.id, person);
        Ok(())
    }
    fn voiceprints(&self, model: &EmbeddingModelId) -> Result<Vec<Voiceprint>, VoiceprintError> {
        Ok(self
            .voiceprints
            .values()
            .filter(|v| &v.model == model)
            .cloned()
            .collect())
    }
    fn voiceprints_of(&self, person: EntityId) -> Result<Vec<Voiceprint>, VoiceprintError> {
        Ok(self
            .voiceprints
            .values()
            .filter(|v| v.person_id == person)
            .cloned()
            .collect())
    }
    fn put_voiceprint(&mut self, voiceprint: Voiceprint) -> Result<(), VoiceprintError> {
        self.voiceprints.insert(voiceprint.id, voiceprint);
        Ok(())
    }
    fn samples(&self, model: &EmbeddingModelId) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        Ok(self.samples_where(|s| s.model() == Some(model)))
    }
    fn samples_of(&self, print: EntityId) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        Ok(self.samples_where(|s| s.voiceprint_id() == Some(print)))
    }
    fn samples_of_person(
        &self,
        person: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        Ok(self.samples_where(|s| s.person_id() == Some(person)))
    }
    fn samples_from(
        &self,
        meeting: EntityId,
        speaker: EntityId,
    ) -> Result<Vec<VoiceprintSample>, VoiceprintError> {
        let origin = SampleOrigin::Meeting { meeting, speaker };
        Ok(self.samples_where(|s| s.origin() == Some(origin)))
    }
    fn sample(&self, id: EntityId) -> Result<Option<VoiceprintSample>, VoiceprintError> {
        Ok(self.samples.get(&id).cloned())
    }
    fn put_sample(&mut self, sample: VoiceprintSample) -> Result<(), VoiceprintError> {
        self.samples.insert(sample.id(), sample);
        Ok(())
    }
}

/// Follows `merged_into` from `id` to the live person it ends at (`None` if the chain ends
/// at a deleted or missing person, or loops).
///
/// # Errors
/// Storage errors.
pub fn resolve_person<R: VoiceprintRepo + ?Sized>(
    repo: &R,
    id: EntityId,
) -> Result<Option<Person>, VoiceprintError> {
    let mut seen = Vec::new();
    let mut cur = id;
    loop {
        if seen.contains(&cur) {
            return Ok(None);
        }
        seen.push(cur);
        match repo.person(cur)? {
            None => return Ok(None),
            Some(p) if p.meta.deleted => return Ok(None),
            Some(p) => match p.merged_into {
                Some(next) => cur = next,
                None => return Ok(Some(p)),
            },
        }
    }
}

/// Marks `from` as merged into `into` (both must be live). Voiceprints stay attached to
/// `from` and match as `into` through [`resolve_person`].
///
/// # Errors
/// [`VoiceprintError::UnknownPerson`] if either is not live, or `from == into`.
pub fn merge_persons<R: VoiceprintRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut R,
    from: EntityId,
    into: EntityId,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<(), VoiceprintError> {
    let live = |p: Option<Person>, id| {
        p.filter(Person::is_live)
            .ok_or(VoiceprintError::UnknownPerson(id))
    };
    let mut a = live(repo.person(from)?, from)?;
    live(repo.person(into)?, into)?;
    if from == into {
        return Err(VoiceprintError::UnknownPerson(from));
    }
    a.merged_into = Some(into);
    a.meta.touch(hlc, clock);
    repo.put_person(a)
}

/// True if `start`'s `merged_into` chain (including `start` itself) reaches `target`.
fn chain_reaches<R: VoiceprintRepo + ?Sized>(
    repo: &R,
    start: EntityId,
    target: EntityId,
) -> Result<bool, VoiceprintError> {
    let mut seen = Vec::new();
    let mut cur = start;
    loop {
        if cur == target {
            return Ok(true);
        }
        if seen.contains(&cur) {
            return Ok(false);
        }
        seen.push(cur);
        match repo.person(cur)?.and_then(|p| p.merged_into) {
            Some(next) => cur = next,
            None => return Ok(false),
        }
    }
}

/// The identity `person`: `person` first, then every person merged into it, directly or
/// through a `merged_into` chain (they match as `person`). Biometric deletion is per
/// identity ([`forget_voiceprints`]); other stores of biometric data follow the same set.
///
/// # Errors
/// Storage errors.
pub fn identity_of<R: VoiceprintRepo + ?Sized>(
    repo: &R,
    person: EntityId,
) -> Result<Vec<EntityId>, VoiceprintError> {
    let mut identity = vec![person];
    for p in repo.persons()? {
        if p.id != person && chain_reaches(repo, p.id, person)? {
            identity.push(p.id);
        }
    }
    Ok(identity)
}

/// What [`forget_voiceprints`] erased.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// Voiceprints tombstoned with their vector zeroed.
    pub voiceprints: usize,
    /// Voice samples tombstoned (no body left).
    pub samples: usize,
}

impl Forgotten {
    /// Every biometric row erased: what decides a checkpoint and what is shown.
    #[must_use]
    pub fn total(self) -> usize {
        self.voiceprints + self.samples
    }
}

impl std::ops::AddAssign for Forgotten {
    fn add_assign(&mut self, other: Self) {
        self.voiceprints += other.voiceprints;
        self.samples += other.samples;
    }
}

/// Deletes every voiceprint of the identity `person` ([`identity_of`]): its own and those of
/// every person merged into it, directly or through a chain, since those match as `person`
/// (biometric data is deletable per person, PLAN.md 3.9). A print is tombstoned with its
/// vector zeroed; a voice sample is tombstoned with no body left, every live sample written
/// for a person of the identity or adding to one of its prints (docs/voice-samples.md 8).
/// Deletion is per identity, not per row.
///
/// # Errors
/// Storage errors.
pub fn forget_voiceprints<R: VoiceprintRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut R,
    person: EntityId,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<Forgotten, VoiceprintError> {
    let identity = identity_of(repo, person)?;
    let mut out = Forgotten::default();
    let mut samples: BTreeMap<EntityId, VoiceprintSample> = BTreeMap::new();
    for &id in &identity {
        for s in repo.samples_of_person(id)? {
            samples.insert(s.id(), s);
        }
        for mut v in repo.voiceprints_of(id)? {
            for s in repo.samples_of(v.id)? {
                samples.insert(s.id(), s);
            }
            if !v.meta.deleted {
                v.meta.mark_deleted(hlc, clock);
                v.sum.fill(0.0);
                repo.put_voiceprint(v)?;
                out.voiceprints += 1;
            }
        }
    }
    for s in samples.into_values() {
        repo.put_sample(sample::erase(&s, hlc, clock)?)?;
        out.samples += 1;
    }
    Ok(out)
}

/// Enrolment settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnrolmentConfig {
    /// Speech needed to create a new voiceprint (default 10 s, PLAN.md 4.0). Updates of an
    /// existing voiceprint accept any amount.
    pub min_new_speech: Duration,
}

impl Default for EnrolmentConfig {
    fn default() -> Self {
        EnrolmentConfig {
            min_new_speech: Duration::from_secs(10),
        }
    }
}

/// The user's name until they give one.
pub const DEFAULT_ME_NAME: &str = "Me";

/// The user's live person: the existing "Me" (`is_me`) if there is one, else a new person
/// named "Me".
///
/// # Errors
/// Storage errors.
pub fn me_person<Repo, C, R>(
    repo: &mut Repo,
    stamp: &mut Stamp<'_, C, R>,
) -> Result<Person, VoiceprintError>
where
    Repo: VoiceprintRepo + ?Sized,
    C: Clock + ?Sized,
    R: Rng + ?Sized,
{
    if let Some(me) = existing_me(repo)? {
        return Ok(me);
    }
    let me = Person::new(DEFAULT_ME_NAME, true, stamp);
    repo.put_person(me.clone())?;
    Ok(me)
}

/// The user's live person if there is one: the live `is_me` person with the lowest id (the
/// one [`me_person`] returns), without creating one.
///
/// # Errors
/// Storage errors.
pub fn existing_me<Repo>(repo: &Repo) -> Result<Option<Person>, VoiceprintError>
where
    Repo: VoiceprintRepo + ?Sized,
{
    Ok(repo
        .persons()?
        .into_iter()
        .filter(|p| p.is_me && p.is_live())
        .min_by_key(|p| p.id))
}

/// "Me" enrolment: enrols `embeddings` for the user's person ([`me_person`]) as samples from
/// `origin`. Returns the person and voiceprint ids. Every check (the embeddings, and the speech
/// a first voiceprint needs) runs before the "Me" person is created, so a refused first
/// enrolment writes nothing.
///
/// # Errors
/// As [`enroll`].
pub fn enroll_me<Repo, C, R>(
    repo: &mut Repo,
    embeddings: &[Embedding],
    speech: Duration,
    origin: SampleOrigin,
    config: &EnrolmentConfig,
    stamp: &mut Stamp<'_, C, R>,
) -> Result<(EntityId, EntityId), VoiceprintError>
where
    Repo: VoiceprintRepo + ?Sized,
    C: Clock + ?Sized,
    R: Rng + ?Sized,
{
    let model = &embeddings
        .first()
        .ok_or(VoiceprintError::NoEmbeddings)?
        .model;
    check_embeddings(embeddings, model, None)?;
    let has_print = match existing_me(repo)? {
        Some(me) => sample::enrolment_print(repo, me.id, model)?
            .is_some_and(|v| sample::effective_of(repo, &v, &[]).is_ok_and(|e| e.count() > 0)),
        None => false,
    };
    if !has_print && speech < config.min_new_speech {
        return Err(VoiceprintError::NotEnoughSpeech {
            needed: config.min_new_speech,
            got: speech,
        });
    }
    let me = me_person(repo, stamp)?;
    let vp = enroll(repo, me.id, embeddings, speech, origin, config, stamp)?;
    Ok((me.id, vp))
}

/// Result of open-set matching.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchOutcome {
    /// The best person reached the threshold.
    Known {
        /// The (resolved, live) person.
        person: EntityId,
        /// Cosine similarity to their best voiceprint.
        score: f32,
    },
    /// Nobody reached the threshold; `best` is the closest person, if any voiceprint of the
    /// model exists.
    Unknown {
        /// Closest person and score.
        best: Option<(EntityId, f32)>,
    },
}

/// [`match_embedding_with_report`]'s result: the outcome and how many stored voiceprints
/// were skipped as invalid.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchReport {
    /// The open-set decision.
    pub outcome: MatchOutcome,
    /// Live voiceprints of the model skipped because their centroid is invalid (zero or
    /// non-finite) or their dimension differs from the embedding's. A print with an
    /// effective count of 0 holds no voice and is neither scored nor counted here.
    pub skipped_invalid: usize,
}

/// Open-set identification of `embedding` against the live voiceprints of its own model
/// (other models are never compared). Each voiceprint scores the cosine similarity to its
/// effective centroid; a merged person scores as the person it was merged into; the best person is
/// `Known` if its score is at least `threshold`. Ties go to the smaller person id. A stored
/// voiceprint that cannot be scored is skipped ([`match_embedding_with_report`] counts them).
///
/// # Errors
/// [`VoiceprintError::Embedding`] for an invalid embedding; storage errors.
pub fn match_embedding<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    embedding: &Embedding,
    threshold: f32,
) -> Result<MatchOutcome, VoiceprintError> {
    Ok(match_embedding_with_report(repo, embedding, threshold)?.outcome)
}

/// [`match_embedding`], also reporting stored voiceprints skipped because they cannot be
/// scored (a corrupt row never fails the whole match).
///
/// # Errors
/// [`VoiceprintError::Embedding`] for an invalid embedding; storage errors.
pub fn match_embedding_with_report<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    embedding: &Embedding,
    threshold: f32,
) -> Result<MatchReport, VoiceprintError> {
    match_embedding_with_margin(repo, embedding, threshold, 0.0)
}

/// [`match_embedding_with_report`] with a margin (T12.67): the best person is `Known` only if
/// its score also exceeds the runner-up person's by at least `margin` (with no runner-up no
/// margin is needed); otherwise the outcome is `Unknown` with the best person. A margin of 0
/// is [`match_embedding_with_report`] exactly. A model's margin comes from
/// [`calibrated_margin`].
///
/// # Errors
/// As [`match_embedding_with_report`].
pub fn match_embedding_with_margin<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    embedding: &Embedding,
    threshold: f32,
    margin: f32,
) -> Result<MatchReport, VoiceprintError> {
    if !embedding.is_valid() {
        return Err(DiarError::Shape("embedding is non-finite or all zeros".to_owned()).into());
    }
    let mut best: BTreeMap<EntityId, f32> = BTreeMap::new();
    let mut skipped_invalid = 0;
    for (v, eff) in live_prints(repo, &embedding.model)? {
        if eff.count() == 0 {
            continue;
        }
        let Some(person) = resolve_person(repo, v.person_id)? else {
            continue;
        };
        let Ok(s) = eff.score(embedding) else {
            skipped_invalid += 1;
            continue;
        };
        let e = best.entry(person.id).or_insert(f32::NEG_INFINITY);
        *e = e.max(s);
    }
    let top = best
        .iter()
        .fold(None::<(EntityId, f32)>, |acc, (&p, &s)| match acc {
            Some((_, bs)) if bs >= s => acc,
            _ => Some((p, s)),
        });
    let runner_up = top.and_then(|(winner, _)| {
        best.iter()
            .filter(|(p, _)| **p != winner)
            .map(|(_, s)| *s)
            .max_by(f32::total_cmp)
    });
    let outcome = match top {
        Some((person, score))
            if score >= threshold && runner_up.is_none_or(|r| score - r >= margin) =>
        {
            MatchOutcome::Known { person, score }
        }
        other => MatchOutcome::Unknown { best: other },
    };
    Ok(MatchReport {
        outcome,
        skipped_invalid,
    })
}

/// An open-set threshold fixed on dev data for one exact embedding model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibratedThreshold {
    /// Exact [`EmbeddingModelId`] string.
    pub model: &'static str,
    /// Cosine-similarity threshold.
    pub threshold: f32,
    /// How it was obtained.
    pub source: &'static str,
}

/// Target false-accept rate of unknown speakers on dev-clean used by the calibration rule:
/// half of the `vp.false_accept` acceptance bound (5%), leaving margin for the shift from
/// dev to test speakers. Fixed before any acceptance run.
pub const DEV_TARGET_FALSE_ACCEPT: f64 = 0.025;

/// Thresholds calibrated on the LibriSpeech dev-clean split only (never on test-clean).
/// Reproduced by `cargo test -p earshot-onnx voiceprint_calibration`, which fails if the
/// value here differs from a fresh calibration.
pub const CALIBRATED_THRESHOLDS: &[CalibratedThreshold] = &[CalibratedThreshold {
    model: "campplus-en-voxceleb@357a834f702b",
    threshold: CAMPPLUS_DEV_CLEAN_THRESHOLD,
    source: "LibriSpeech dev-clean, 40 speakers (fixtures/librispeech/subset.toml), 2 folds of \
             20 enrolled / 20 unknown, 10 s enrolment, 2.5 s test clips, unknown false-accept \
             rate <= DEV_TARGET_FALSE_ACCEPT",
}];

/// The CAM++ VoxCeleb threshold from the dev-clean calibration (see
/// [`CALIBRATED_THRESHOLDS`]). Dev-clean at this value: unknown false accepts 3/120 (2.5%),
/// known clips top-1 120/120, top-1 and accepted 119/120.
pub const CAMPPLUS_DEV_CLEAN_THRESHOLD: f32 = 0.461_961_03;

/// A margin over the runner-up person ([`match_embedding_with_margin`]) chosen on dev meeting
/// audio for one exact embedding model (T12.67).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibratedMargin {
    /// Exact [`EmbeddingModelId`] string.
    pub model: &'static str,
    /// Cosine margin.
    pub margin: f32,
    /// How it was obtained.
    pub source: &'static str,
}

/// Margins chosen by the rule in `docs/voiceprint-meetings.md` on AMI dev meetings (never an
/// acceptance meeting), measured by `cargo run --release -p earshot-runtime --example
/// vp_meetings -- <repo> --models <dir>`.
pub const CALIBRATED_MARGINS: &[CalibratedMargin] = &[CalibratedMargin {
    model: "campplus-en-voxceleb@357a834f702b",
    margin: CAMPPLUS_MEETING_MARGIN,
    source: "T12.67 rule (docs/voiceprint-meetings.md): AMI dev series, enrolled from each \
             first meeting's diarised centroids, 40 voices of the second and third meetings; \
             the smallest margin on 0.00-0.20 step 0.02 with false accepts <= 5% in both \
             conditions at the 0.462 threshold",
}];

/// The CAM++ VoxCeleb margin (see [`CALIBRATED_MARGINS`]), measured 2026-11-16: at 0.08 the
/// meetings' unknown voices were named 1/40 times (20% with no margin) and known voices wrongly
/// 1/40 (2/40), with no known voice missed (0/38 either way).
pub const CAMPPLUS_MEETING_MARGIN: f32 = 0.08;

/// The margin chosen for `model`, or `None` if it has none (it is then matched by its threshold
/// alone). A margin acts only when two or more persons are stored (with one there is no
/// runner-up), and the final pass never lets it take the user's own name away (a best match
/// to the user's identity at the threshold stands).
#[must_use]
pub fn calibrated_margin(model: &EmbeddingModelId) -> Option<f32> {
    CALIBRATED_MARGINS
        .iter()
        .find(|c| c.model == model.as_str())
        .map(|c| c.margin)
}

/// The calibrated open-set threshold for `model`.
///
/// # Errors
/// [`VoiceprintError::NoThreshold`] if `model` has none (a new or changed model must be
/// calibrated on dev data first).
pub fn calibrated_threshold(model: &EmbeddingModelId) -> Result<f32, VoiceprintError> {
    CALIBRATED_THRESHOLDS
        .iter()
        .find(|c| c.model == model.as_str())
        .map(|c| c.threshold)
        .ok_or_else(|| VoiceprintError::NoThreshold(model.clone()))
}

/// The calibration rule: the threshold at which at most `floor(target * n)` of the `n`
/// impostor scores (best scores of unknown speakers' clips against the enrolled
/// voiceprints) are accepted (`score >= threshold`): the midpoint between the last rejected
/// and the first accepted impostor score in descending order (between the top score and 1
/// when none may be accepted).
///
/// # Panics
/// If `impostor` is empty or contains a non-finite score, or `target` is outside `[0, 1)`.
#[must_use]
pub fn calibrate_threshold(impostor: &[f32], target: f64) -> f32 {
    assert!(!impostor.is_empty(), "no impostor scores");
    assert!(impostor.iter().all(|s| s.is_finite()), "non-finite score");
    assert!((0.0..1.0).contains(&target), "target must be in [0, 1)");
    let mut s = impostor.to_vec();
    s.sort_by(|a, b| b.total_cmp(a));
    let allowed = (target * s.len() as f64).floor() as usize;
    let upper = if allowed == 0 { 1.0 } else { s[allowed - 1] };
    let lower = s[allowed];
    if upper == lower {
        // Ties straddle the cut: reject them all.
        return lower.next_up();
    }
    lower + (upper - lower) / 2.0
}
