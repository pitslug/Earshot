//! Voice samples and the effective print (T12.77b, docs/voice-samples.md 2 and 3).
//!
//! A [`VoiceprintSample`] records one enrolled embedding: the print it adds to, that print's
//! person, the unit vector, the speech behind it and the meeting voice it came from. A print's
//! effective sum ([`effective`]) is its stored base plus its live samples in
//! `(meta.created_at, id)` order, so taking one sample back restores the print exactly.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    EnrolmentConfig, Voiceprint, VoiceprintError, VoiceprintRepo, check_embeddings, identity_of,
    resolve_person, unit,
};
use crate::diar::{DiarError, Embedding, EmbeddingModelId};
use crate::identity::Hlc;
use crate::identity::{Clock, EntityId, SyncClass, SyncMeta, SyncSerialize, SyncableOptInClass};
use crate::voiceprint::Stamp;
use rand_core::Rng;
use std::time::Duration;

/// The largest embedding dimension a sample may hold (CAM++ is 512). 4096 values printed at
/// full JSON length stay inside the sync envelope's plaintext limit.
pub const MAX_EMBEDDING_DIM: usize = 4096;

/// The most speech one sample may record (24 h, within SQLite's `INTEGER`). Writers clamp to
/// it.
pub const MAX_SAMPLE_SPEECH_MS: u64 = 86_400_000;

/// How far a live sample's Euclidean norm may be from 1.
pub const SAMPLE_NORM_TOLERANCE: f32 = 1e-3;

/// Where a sample's voice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleOrigin {
    /// A voice of a meeting: the meeting and its speaker row.
    Meeting {
        /// The meeting.
        meeting: EntityId,
        /// Its speaker row.
        speaker: EntityId,
    },
    /// The user's own voice sample ("Your voice").
    Own,
}

/// A sample row as stored or sent: every field, the body ones empty for a tombstone. The one
/// input of [`VoiceprintSample::from_stored`] and the sample's wire shape.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleRecord {
    /// Row id.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The print it adds to (`None` for a tombstone).
    pub voiceprint_id: Option<EntityId>,
    /// That print's person when written (`None` for a tombstone).
    pub person_id: Option<EntityId>,
    /// The embedding model (`None` for a tombstone).
    pub model: Option<EmbeddingModelId>,
    /// The unit vector (empty for a tombstone).
    pub vector: Vec<f32>,
    /// Speech behind it in milliseconds (`None` for a tombstone).
    pub speech_ms: Option<u64>,
    /// The meeting it came from (`None` for "Your voice" and a tombstone).
    pub meeting_id: Option<EntityId>,
    /// The meeting's speaker row (set exactly when `meeting_id` is).
    pub speaker_id: Option<EntityId>,
}

impl fmt::Debug for SampleRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the vector (biometric data).
        f.debug_struct("SampleRecord")
            .field("id", &self.id)
            .field("meta", &self.meta)
            .field("voiceprint_id", &self.voiceprint_id)
            .field("person_id", &self.person_id)
            .field("model", &self.model)
            .field("dim", &self.vector.len())
            .field("speech_ms", &self.speech_ms)
            .field("meeting_id", &self.meeting_id)
            .field("speaker_id", &self.speaker_id)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
struct SampleBody {
    voiceprint_id: EntityId,
    person_id: EntityId,
    model: EmbeddingModelId,
    vector: Vec<f32>,
    speech_ms: u64,
    meeting_id: Option<EntityId>,
    speaker_id: Option<EntityId>,
}

/// One enrolled embedding of a voiceprint (docs/voice-samples.md 2). Written once, never
/// changed while live; its only later write is the tombstone, which keeps nothing but its id
/// and sync metadata. Built only through [`VoiceprintSample::new`],
/// [`VoiceprintSample::erased`] and [`VoiceprintSample::from_stored`], all checked.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "SampleRecord", try_from = "SampleRecord")]
pub struct VoiceprintSample {
    id: EntityId,
    meta: SyncMeta,
    // `None` exactly when `meta.deleted` (a tombstone).
    body: Option<SampleBody>,
}

impl fmt::Debug for VoiceprintSample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the vector (biometric data).
        let mut d = f.debug_struct("VoiceprintSample");
        d.field("id", &self.id).field("meta", &self.meta);
        if let Some(b) = &self.body {
            d.field("model", &b.model).field("dim", &b.vector.len());
        }
        d.finish()
    }
}

impl SyncClass for VoiceprintSample {
    type Class = SyncableOptInClass;
}

impl SyncSerialize for VoiceprintSample {
    const ENTITY: &'static str = "voiceprint_sample";
    fn sync_id(&self) -> EntityId {
        self.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

impl From<VoiceprintSample> for SampleRecord {
    fn from(s: VoiceprintSample) -> Self {
        let (voiceprint_id, person_id, model, vector, speech_ms, meeting_id, speaker_id) =
            match s.body {
                Some(b) => (
                    Some(b.voiceprint_id),
                    Some(b.person_id),
                    Some(b.model),
                    b.vector,
                    Some(b.speech_ms),
                    b.meeting_id,
                    b.speaker_id,
                ),
                None => (None, None, None, Vec::new(), None, None, None),
            };
        SampleRecord {
            id: s.id,
            meta: s.meta,
            voiceprint_id,
            person_id,
            model,
            vector,
            speech_ms,
            meeting_id,
            speaker_id,
        }
    }
}

impl TryFrom<SampleRecord> for VoiceprintSample {
    type Error = VoiceprintError;
    fn try_from(r: SampleRecord) -> Result<Self, Self::Error> {
        VoiceprintSample::from_stored(r)
    }
}

fn invalid(why: impl Into<String>) -> VoiceprintError {
    VoiceprintError::InvalidSample(why.into())
}

/// The one check every sample passes (docs/voice-samples.md 2.1). A tombstone is accepted
/// whatever its record holds and keeps no body.
fn check(r: SampleRecord) -> Result<VoiceprintSample, VoiceprintError> {
    if r.meta.deleted {
        return Ok(VoiceprintSample {
            id: r.id,
            meta: r.meta,
            body: None,
        });
    }
    let (Some(voiceprint_id), Some(person_id), Some(model), Some(speech_ms)) =
        (r.voiceprint_id, r.person_id, r.model, r.speech_ms)
    else {
        return Err(invalid(
            "a live sample lacks its print, person, model or speech",
        ));
    };
    let dim = r.vector.len();
    if dim == 0 || dim > MAX_EMBEDDING_DIM {
        return Err(invalid(format!(
            "sample dimension {dim} outside 1 to {MAX_EMBEDDING_DIM}"
        )));
    }
    if r.vector.iter().any(|x| !x.is_finite()) {
        return Err(invalid("sample vector is not finite"));
    }
    let norm = r
        .vector
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    if (norm - 1.0).abs() > f64::from(SAMPLE_NORM_TOLERANCE) {
        return Err(invalid("sample vector is not of unit length"));
    }
    if speech_ms > MAX_SAMPLE_SPEECH_MS {
        return Err(invalid("sample speech above the limit"));
    }
    if r.meeting_id.is_some() != r.speaker_id.is_some() {
        return Err(invalid("sample meeting and speaker must be set together"));
    }
    Ok(VoiceprintSample {
        id: r.id,
        meta: r.meta,
        body: Some(SampleBody {
            voiceprint_id,
            person_id,
            model,
            vector: r.vector,
            speech_ms,
            meeting_id: r.meeting_id,
            speaker_id: r.speaker_id,
        }),
    })
}

impl VoiceprintSample {
    /// Tests only: a live sample from `record` with no check at all, so the fold can be shown
    /// to survive a row no checked constructor would build (a non-finite vector).
    #[cfg(test)]
    pub(super) fn unchecked_for_test(r: SampleRecord) -> Self {
        let body = (|| {
            Some(SampleBody {
                voiceprint_id: r.voiceprint_id?,
                person_id: r.person_id?,
                model: r.model?,
                vector: r.vector,
                speech_ms: r.speech_ms?,
                meeting_id: r.meeting_id,
                speaker_id: r.speaker_id,
            })
        })();
        VoiceprintSample {
            id: r.id,
            meta: r.meta,
            body,
        }
    }

    /// A new live sample adding `vector` (made unit length here) and `speech_ms` (clamped to
    /// [`MAX_SAMPLE_SPEECH_MS`]) to `print`, from `origin`.
    ///
    /// # Errors
    /// [`VoiceprintError::InvalidSample`] when the vector fails the check (wrong dimension,
    /// not finite, all zeros) or differs from the print's model or dimension.
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        print: &Voiceprint,
        vector: &[f32],
        speech_ms: u64,
        origin: SampleOrigin,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Result<Self, VoiceprintError> {
        if vector.len() != print.sum.len() {
            return Err(invalid("sample dimension differs from its print"));
        }
        let vector = unit(vector).ok_or_else(|| invalid("sample vector is not finite or zero"))?;
        let (meeting_id, speaker_id) = match origin {
            SampleOrigin::Meeting { meeting, speaker } => (Some(meeting), Some(speaker)),
            SampleOrigin::Own => (None, None),
        };
        let (id, meta) = stamp.new_row();
        check(SampleRecord {
            id,
            meta,
            voiceprint_id: Some(print.id),
            person_id: Some(print.person_id),
            model: Some(print.model.clone()),
            vector,
            speech_ms: Some(speech_ms.min(MAX_SAMPLE_SPEECH_MS)),
            meeting_id,
            speaker_id,
        })
    }

    /// The tombstone of sample `id`: only its id and `meta`, which must be marked deleted.
    ///
    /// # Errors
    /// [`VoiceprintError::InvalidSample`] if `meta` is not a tombstone's.
    pub fn erased(id: EntityId, meta: SyncMeta) -> Result<Self, VoiceprintError> {
        if !meta.deleted {
            return Err(invalid("an erased sample must be marked deleted"));
        }
        check(SampleRecord {
            id,
            meta,
            voiceprint_id: None,
            person_id: None,
            model: None,
            vector: Vec::new(),
            speech_ms: None,
            meeting_id: None,
            speaker_id: None,
        })
    }

    /// A sample as stored or pulled, through the one check (docs/voice-samples.md 2.1).
    ///
    /// # Errors
    /// [`VoiceprintError::InvalidSample`] for a live record that fails it.
    pub fn from_stored(record: SampleRecord) -> Result<Self, VoiceprintError> {
        check(record)
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

    /// True for a tombstone (it has no body).
    #[must_use]
    pub fn is_erased(&self) -> bool {
        self.body.is_none()
    }

    /// The print it adds to (`None` for a tombstone).
    #[must_use]
    pub fn voiceprint_id(&self) -> Option<EntityId> {
        self.body.as_ref().map(|b| b.voiceprint_id)
    }

    /// The print's person when it was written (`None` for a tombstone).
    #[must_use]
    pub fn person_id(&self) -> Option<EntityId> {
        self.body.as_ref().map(|b| b.person_id)
    }

    /// The embedding model (`None` for a tombstone).
    #[must_use]
    pub fn model(&self) -> Option<&EmbeddingModelId> {
        self.body.as_ref().map(|b| &b.model)
    }

    /// The unit vector (`None` for a tombstone).
    #[must_use]
    pub fn vector(&self) -> Option<&[f32]> {
        self.body.as_ref().map(|b| b.vector.as_slice())
    }

    /// The speech it adds in milliseconds (`None` for a tombstone).
    #[must_use]
    pub fn speech_ms(&self) -> Option<u64> {
        self.body.as_ref().map(|b| b.speech_ms)
    }

    /// Where its voice came from (`None` for a tombstone).
    #[must_use]
    pub fn origin(&self) -> Option<SampleOrigin> {
        self.body
            .as_ref()
            .map(|b| match (b.meeting_id, b.speaker_id) {
                (Some(meeting), Some(speaker)) => SampleOrigin::Meeting { meeting, speaker },
                _ => SampleOrigin::Own,
            })
    }
}

/// A print's effective state: its stored base plus its live samples ([`effective`]). The one
/// thing matching, enrolment checks and the app read about a print.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    model: EmbeddingModelId,
    sum: Vec<f32>,
    count: u32,
    speech_ms: u64,
    skipped: usize,
    base_count: u32,
}

impl Effective {
    /// The unit-length centroid (normalised effective sum), or `None` when the sum is zero or
    /// not finite.
    #[must_use]
    pub fn centroid(&self) -> Option<Embedding> {
        unit(&self.sum).map(|v| Embedding::new(self.model.clone(), v))
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

    /// True when the print holds enrolments from before samples existed (a non-zero base).
    #[must_use]
    pub fn has_base(&self) -> bool {
        self.base_count > 0
    }

    /// The embedding model.
    #[must_use]
    pub fn model(&self) -> &EmbeddingModelId {
        &self.model
    }

    /// The effective sum of unit embeddings.
    #[must_use]
    pub fn sum(&self) -> &[f32] {
        &self.sum
    }

    /// Embeddings behind it (base plus live samples, saturating). A print with count 0 holds
    /// no voice: it is not scored and reads as no voiceprint.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Speech behind it in milliseconds (base plus live samples, saturating).
    #[must_use]
    pub fn speech_ms(&self) -> u64 {
        self.speech_ms
    }

    /// Live samples of the print left out because they cannot add to it (another model or
    /// dimension, a non-finite vector, or another person).
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.skipped
    }
}

/// `print`'s effective state: the stored base plus each live sample of that print among
/// `samples`, in `(meta.created_at, id)` order whatever the input order, the vector added in
/// plain f32 and count and speech saturating (docs/voice-samples.md 3.1). Samples of other
/// prints and tombstones are ignored; samples of another model or dimension, non-finite ones
/// and ones of another person are skipped and counted. Every device with the same rows gets
/// the same sum bit for bit.
#[must_use]
pub fn effective(print: &Voiceprint, samples: &[VoiceprintSample]) -> Effective {
    fold(print, samples, &[])
}

/// [`effective`] with the samples in `excluding` left out.
pub(super) fn fold(
    print: &Voiceprint,
    samples: &[VoiceprintSample],
    excluding: &[EntityId],
) -> Effective {
    let mut mine: Vec<(&VoiceprintSample, &SampleBody)> = samples
        .iter()
        .filter(|s| !s.meta.deleted && !excluding.contains(&s.id))
        .filter_map(|s| s.body.as_ref().map(|b| (s, b)))
        .filter(|(_, b)| b.voiceprint_id == print.id)
        .collect();
    mine.sort_by_key(|(s, _)| (s.meta.created_at, s.id));
    let mut out = Effective {
        model: print.model.clone(),
        sum: print.sum.clone(),
        count: print.count,
        speech_ms: print.speech_ms,
        skipped: 0,
        base_count: print.count,
    };
    for (_, b) in mine {
        if b.model != print.model
            || b.vector.len() != out.sum.len()
            || b.vector.iter().any(|x| !x.is_finite())
            || b.person_id != print.person_id
        {
            out.skipped += 1;
            continue;
        }
        for (s, x) in out.sum.iter_mut().zip(&b.vector) {
            *s += x;
        }
        out.count = out.count.saturating_add(1);
        out.speech_ms = out.speech_ms.saturating_add(b.speech_ms);
    }
    out
}

/// The live voiceprints of `model` with their effective state, folding the stored samples
/// of the model. Prints with an effective count of 0 are included: callers that score or count
/// voices leave them out.
///
/// # Errors
/// Storage errors.
pub fn live_prints<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    model: &EmbeddingModelId,
) -> Result<Vec<(Voiceprint, Effective)>, VoiceprintError> {
    let samples = repo.samples(model)?;
    Ok(repo
        .voiceprints(model)?
        .into_iter()
        .filter(|v| !v.meta.deleted && &v.model == model)
        .map(|v| {
            let e = fold(&v, &samples, &[]);
            (v, e)
        })
        .collect())
}

/// The live voiceprints of `person` (any model, not following merges) with their effective
/// state, folding each print's stored samples, as [`live_prints`].
///
/// # Errors
/// Storage errors.
pub fn live_prints_of<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    person: EntityId,
) -> Result<Vec<(Voiceprint, Effective)>, VoiceprintError> {
    repo.voiceprints_of(person)?
        .into_iter()
        .filter(|v| !v.meta.deleted)
        .map(|v| {
            let e = fold(&v, &repo.samples_of(v.id)?, &[]);
            Ok((v, e))
        })
        .collect()
}

/// The print an enrolment of `model` for `person` goes to (docs/voice-samples.md 4): the
/// person's live print of that model with the earliest `(meta.created_at, id)`, so every
/// computer picks the same of two prints two computers each made; `None` when there is none.
/// The one choice [`admissible`] judges and [`enroll`] writes to.
pub(super) fn enrolment_print<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    person: EntityId,
    model: &EmbeddingModelId,
) -> Result<Option<Voiceprint>, VoiceprintError> {
    Ok(repo
        .voiceprints_of(person)?
        .into_iter()
        .filter(|v| &v.model == model && !v.meta.deleted)
        .min_by_key(|v| (v.meta.created_at, v.id)))
}

/// `print` folded with its stored samples, those in `excluding` left out.
pub(super) fn effective_of<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    print: &Voiceprint,
    excluding: &[EntityId],
) -> Result<Effective, VoiceprintError> {
    Ok(fold(print, &repo.samples_of(print.id)?, excluding))
}

/// The unit-length copy of `v` exactly as an enrolment stores it in a sample (f64
/// accumulation), or `None` when it is all zeros or not finite: what a re-save compares a
/// stored sample with, bit for bit.
#[must_use]
pub fn unit_vector(v: &[f32]) -> Option<Vec<f32>> {
    unit(v)
}

/// Every check [`enroll`] makes, without writing: `person` live, the embeddings of one
/// model and dimension and valid, and, for a print with an effective count of 0 or none yet,
/// at least `config.min_new_speech`. The samples in `excluding` are left out of the effective
/// count (a re-save judges the voice without its own earlier sample).
///
/// # Errors
/// As [`enroll`].
pub fn admissible<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    person: EntityId,
    embeddings: &[Embedding],
    speech: Duration,
    config: &EnrolmentConfig,
    excluding: &[EntityId],
) -> Result<(), VoiceprintError> {
    if !repo.person(person)?.is_some_and(|p| p.is_live()) {
        return Err(VoiceprintError::UnknownPerson(person));
    }
    let model = &embeddings
        .first()
        .ok_or(VoiceprintError::NoEmbeddings)?
        .model;
    let existing = match enrolment_print(repo, person, model)? {
        Some(v) => {
            let e = effective_of(repo, &v, excluding)?;
            Some((v.sum.len(), e.count))
        }
        None => None,
    };
    match existing {
        Some((dim, count)) if count > 0 => {
            check_embeddings(embeddings, model, Some(dim))?;
        }
        other => {
            if speech < config.min_new_speech {
                return Err(VoiceprintError::NotEnoughSpeech {
                    needed: config.min_new_speech,
                    got: speech,
                });
            }
            check_embeddings(embeddings, model, other.map(|(dim, _)| dim))?;
        }
    }
    Ok(())
}

/// The tombstone of the live sample `s`, stamped now.
pub(super) fn erase<C: Clock + ?Sized>(
    s: &VoiceprintSample,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<VoiceprintSample, VoiceprintError> {
    let mut meta = s.meta;
    meta.mark_deleted(hlc, clock);
    VoiceprintSample::erased(s.id, meta)
}

/// Erases the samples `ids` (docs/voice-samples.md 2): each live one is written as a tombstone
/// stamped now, keeping only its id and sync metadata; tombstones and unknown ids are left as
/// they are. Returns how many were erased; the caller checkpoints after the commit when that
/// is not 0. The one sample erasure every core and runtime writer goes through, except the
/// store's convergence rule 3 (dead-print repair), which erases by id in SQL so a damaged
/// row it could not read is erased too.
///
/// # Errors
/// Storage errors.
pub fn erase_samples<Repo: VoiceprintRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut Repo,
    ids: &[EntityId],
    hlc: &mut Hlc,
    clock: &C,
) -> Result<usize, VoiceprintError> {
    let mut n = 0;
    for &id in ids {
        if let Some(s) = repo.sample(id)?.filter(|s| !s.is_erased()) {
            repo.put_sample(erase(&s, hlc, clock)?)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Takes the voice `speaker` of `meeting` back out (docs/voice-samples.md 6): writes the
/// tombstone of every live sample from that origin whose person, resolved through merges, is
/// not `keep`, itself resolved through merges (`None` takes back all). Keyed only on the sample's own meeting and speaker, so
/// it needs no identity record. Returns how many were taken back; the caller checkpoints after
/// the commit when that is not 0.
///
/// # Errors
/// Storage errors.
pub fn take_back<Repo: VoiceprintRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut Repo,
    meeting: EntityId,
    speaker: EntityId,
    keep: Option<EntityId>,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<usize, VoiceprintError> {
    let keep = match keep {
        Some(p) => resolve_person(repo, p)?.map(|p| p.id),
        None => None,
    };
    let mut gone = Vec::new();
    for s in repo.samples_from(meeting, speaker)? {
        let resolved = match s.person_id() {
            Some(p) => resolve_person(repo, p)?.map(|p| p.id),
            None => None,
        };
        if keep.is_some() && resolved == keep {
            continue;
        }
        gone.push(s.id());
    }
    erase_samples(repo, &gone, hlc, clock)
}

/// Adds `embeddings` (from `speech` of the person's audio, from `origin`) to `person`'s
/// voiceprint of their model as voice samples (docs/voice-samples.md 4); the print's stored base
/// is never added to again. Every check is [`admissible`]'s, run first; a print row is written
/// only when the person has no live print of the model (a zero base: count 0, speech 0); with
/// two live prints (two computers each made one) it adds to [`enrolment_print`]'s choice, the
/// earliest; then one sample per embedding in input order, with increasing stamps, the speech
/// split so the samples sum to it exactly (each `speech / n` ms, the first also the
/// remainder). Its effective print equals what adding to the base gave before samples, bit for
/// bit. Returns the print id.
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
    origin: SampleOrigin,
    config: &EnrolmentConfig,
    stamp: &mut Stamp<'_, C, R>,
) -> Result<EntityId, VoiceprintError>
where
    Repo: VoiceprintRepo + ?Sized,
    C: Clock + ?Sized,
    R: Rng + ?Sized,
{
    admissible(repo, person, embeddings, speech, config, &[])?;
    let model = embeddings[0].model.clone();
    let print = match enrolment_print(repo, person, &model)? {
        Some(v) => v,
        None => {
            let (id, meta) = stamp.new_row();
            let v = Voiceprint {
                id,
                meta,
                person_id: person,
                model,
                sum: vec![0.0; embeddings[0].vector.len()],
                count: 0,
                speech_ms: 0,
            };
            repo.put_voiceprint(v.clone())?;
            v
        }
    };
    let total = u64::try_from(speech.as_millis()).unwrap_or(u64::MAX);
    let n = embeddings.len() as u64;
    for (i, e) in embeddings.iter().enumerate() {
        let share = total / n + if i == 0 { total % n } else { 0 };
        let s = VoiceprintSample::new(&print, &e.vector, share, origin, stamp)?;
        repo.put_sample(s)?;
    }
    Ok(print.id)
}

/// Whether a voice was saved on this computer before samples existed and still counts in
/// `person`'s print (docs/voice-samples.md 5.2): this computer's frozen marker (`marked`, the
/// identity entry's `enrolled`), no live sample from `(meeting, speaker)` in any identity, and
/// a live print of `model` in `person`'s identity with a base. A print forgotten and made again
/// has no base, so a voice saved before then no longer counts.
///
/// # Errors
/// Storage errors.
pub fn saved_before_samples<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    marked: bool,
    meeting: EntityId,
    speaker: EntityId,
    person: EntityId,
    model: &EmbeddingModelId,
) -> Result<bool, VoiceprintError> {
    if !marked || !repo.samples_from(meeting, speaker)?.is_empty() {
        return Ok(false);
    }
    for id in identity_of(repo, person)? {
        if live_prints_of(repo, id)?
            .iter()
            .any(|(v, e)| &v.model == model && e.has_base())
        {
            return Ok(true);
        }
    }
    Ok(false)
}
