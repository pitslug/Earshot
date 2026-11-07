//! Voiceprints (PLAN.md 3.5, T3.4): recurring people, their speaker embeddings, enrolment and
//! open-set matching.
//!
//! - [`Person`]: a recurring identity (syncable) with optional `merged_into`, so duplicates
//!   created on two devices can be merged; [`resolve_person`] follows the chain.
//! - [`Voiceprint`]: the running centroid of one person's embeddings from **one** embedding
//!   model (sync class opt-in, D6). Embeddings of other models are never compared with it.
//! - [`VoiceprintRepo`]: storage trait; [`InMemoryVoiceprintRepo`] for tests (SQLite lives in
//!   `earshot-store`).
//! - [`enroll`], [`enroll_me`]: create or update a voiceprint (running centroid update).
//! - [`match_embedding`]: open-set identification: the best-scoring person (cosine
//!   similarity to the voiceprint centroid) if the score reaches the threshold, otherwise
//!   "unknown".
//! - [`calibrated_threshold`]: the open-set thresholds fixed on the LibriSpeech dev-clean
//!   split only ([`calibrate_threshold`] is the rule; PLAN.md 4.0, AGENTS.md invariant), per
//!   exact embedding-model id.
//!
//! Running centroid: a voiceprint keeps the sum of its unit-length embeddings and their count;
//! its centroid is the normalised sum, so every enrolment embedding has equal weight and an
//! update is O(dimension).

#[cfg(test)]
mod tests;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Voiceprint {
    /// Row id.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The person.
    pub person_id: EntityId,
    /// The embedding model; only embeddings of this exact model are compared.
    pub model: EmbeddingModelId,
    /// Sum of the unit-length enrolment embeddings.
    pub sum: Vec<f32>,
    /// Number of embeddings in `sum`.
    pub count: u32,
    /// Speech behind the embeddings (milliseconds).
    pub speech_ms: u64,
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
    /// The unit-length centroid (normalised sum of the enrolment embeddings).
    #[must_use]
    pub fn centroid(&self) -> Option<Embedding> {
        unit(&self.sum).map(|v| Embedding::new(self.model.clone(), v))
    }

    /// Running centroid update: adds unit-length copies of `embeddings` (all of this
    /// voiceprint's model and dimension) and `speech`.
    ///
    /// # Errors
    /// [`VoiceprintError::Embedding`] for another model, another dimension or an invalid
    /// vector; nothing is changed then.
    pub fn add<C: Clock + ?Sized>(
        &mut self,
        embeddings: &[Embedding],
        speech: Duration,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<(), VoiceprintError> {
        let units = check_embeddings(embeddings, &self.model, Some(self.sum.len()))?;
        for u in &units {
            for (s, x) in self.sum.iter_mut().zip(u) {
                *s += x;
            }
        }
        self.count += units.len() as u32;
        self.speech_ms += speech.as_millis() as u64;
        self.meta.touch(hlc, clock);
        Ok(())
    }

    /// Cosine similarity of `e` to the centroid.
    ///
    /// # Errors
    /// [`DiarError::ModelMismatch`] (wrapped) if `e` comes from another model: such
    /// embeddings are never compared; [`DiarError::Shape`] for another dimension or an
    /// invalid centroid.
    pub fn score(&self, e: &Embedding) -> Result<f32, VoiceprintError> {
        let c = self
            .centroid()
            .ok_or_else(|| DiarError::Shape("voiceprint centroid is invalid".to_owned()))?;
        Ok(c.cosine_similarity(e)?)
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
}

/// In-memory [`VoiceprintRepo`] (tests, and the core pipeline before storage exists).
#[derive(Debug, Clone, Default)]
pub struct InMemoryVoiceprintRepo {
    persons: BTreeMap<EntityId, Person>,
    voiceprints: BTreeMap<EntityId, Voiceprint>,
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

/// Deletes (tombstones, vector zeroed) every voiceprint of the identity `person`
/// ([`identity_of`]): its own and those of every person merged into it, directly or through
/// a chain, since those match as `person` (biometric data is deletable per person, PLAN.md
/// 3.9). Deletion is per identity, not per row. Returns how many voiceprints were deleted.
///
/// # Errors
/// Storage errors.
pub fn forget_voiceprints<R: VoiceprintRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut R,
    person: EntityId,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<usize, VoiceprintError> {
    let identity = identity_of(repo, person)?;
    let mut n = 0;
    for id in identity {
        for mut v in repo.voiceprints_of(id)? {
            if !v.meta.deleted {
                v.meta.mark_deleted(hlc, clock);
                v.sum.fill(0.0);
                repo.put_voiceprint(v)?;
                n += 1;
            }
        }
    }
    Ok(n)
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

/// Adds `embeddings` (from `speech` of the person's audio) to `person`'s live voiceprint for
/// their model, creating it if there is none. Returns the voiceprint id.
///
/// # Errors
/// [`VoiceprintError::UnknownPerson`] if the person is not live,
/// [`VoiceprintError::NotEnoughSpeech`] for a new voiceprint with too little speech,
/// [`VoiceprintError::Embedding`] for mixed models, dimensions or invalid vectors.
pub fn enroll<Repo, C, R>(
    repo: &mut Repo,
    person: EntityId,
    embeddings: &[Embedding],
    speech: Duration,
    config: &EnrolmentConfig,
    stamp: &mut Stamp<'_, C, R>,
) -> Result<EntityId, VoiceprintError>
where
    Repo: VoiceprintRepo + ?Sized,
    C: Clock + ?Sized,
    R: Rng + ?Sized,
{
    if !repo.person(person)?.is_some_and(|p| p.is_live()) {
        return Err(VoiceprintError::UnknownPerson(person));
    }
    let model = embeddings
        .first()
        .ok_or(VoiceprintError::NoEmbeddings)?
        .model
        .clone();
    let existing = repo
        .voiceprints_of(person)?
        .into_iter()
        .find(|v| v.model == model && !v.meta.deleted);
    if let Some(mut v) = existing {
        v.add(embeddings, speech, stamp.hlc, stamp.clock)?;
        let id = v.id;
        repo.put_voiceprint(v)?;
        return Ok(id);
    }
    if speech < config.min_new_speech {
        return Err(VoiceprintError::NotEnoughSpeech {
            needed: config.min_new_speech,
            got: speech,
        });
    }
    let units = check_embeddings(embeddings, &model, None)?;
    let dim = units[0].len();
    let mut sum = vec![0.0f32; dim];
    for u in &units {
        for (s, x) in sum.iter_mut().zip(u) {
            *s += x;
        }
    }
    let (id, meta) = stamp.new_row();
    repo.put_voiceprint(Voiceprint {
        id,
        meta,
        person_id: person,
        model,
        sum,
        count: units.len() as u32,
        speech_ms: speech.as_millis() as u64,
    })?;
    Ok(id)
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

/// "Me" enrolment: enrols `embeddings` for the user's person ([`me_person`]). Returns the
/// person and voiceprint ids. Every check (the embeddings, and the speech a first voiceprint
/// needs) runs before the "Me" person is created, so a refused first enrolment writes
/// nothing.
///
/// # Errors
/// As [`enroll`].
pub fn enroll_me<Repo, C, R>(
    repo: &mut Repo,
    embeddings: &[Embedding],
    speech: Duration,
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
        Some(me) => repo
            .voiceprints_of(me.id)?
            .iter()
            .any(|v| &v.model == model && !v.meta.deleted),
        None => false,
    };
    if !has_print && speech < config.min_new_speech {
        return Err(VoiceprintError::NotEnoughSpeech {
            needed: config.min_new_speech,
            got: speech,
        });
    }
    let me = me_person(repo, stamp)?;
    let vp = enroll(repo, me.id, embeddings, speech, config, stamp)?;
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
    /// non-finite) or their dimension differs from the embedding's.
    pub skipped_invalid: usize,
}

/// Open-set identification of `embedding` against the live voiceprints of its own model
/// (other models are never compared). Each voiceprint scores the cosine similarity to its
/// centroid; a merged person scores as the person it was merged into; the best person is
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
    if !embedding.is_valid() {
        return Err(DiarError::Shape("embedding is non-finite or all zeros".to_owned()).into());
    }
    let mut best: BTreeMap<EntityId, f32> = BTreeMap::new();
    let mut skipped_invalid = 0;
    for v in repo.voiceprints(&embedding.model)? {
        if v.meta.deleted || v.model != embedding.model {
            continue;
        }
        let Some(person) = resolve_person(repo, v.person_id)? else {
            continue;
        };
        let Ok(s) = v.score(embedding) else {
            skipped_invalid += 1;
            continue;
        };
        let e = best.entry(person.id).or_insert(f32::NEG_INFINITY);
        *e = e.max(s);
    }
    let top = best
        .into_iter()
        .fold(None::<(EntityId, f32)>, |acc, (p, s)| match acc {
            Some((_, bs)) if bs >= s => acc,
            _ => Some((p, s)),
        });
    let outcome = match top {
        Some((person, score)) if score >= threshold => MatchOutcome::Known { person, score },
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
