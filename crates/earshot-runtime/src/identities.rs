//! Speaker identities kept across final-pass runs (T6.25): a later run of the cleanup on
//! the same meeting matches its new clusters to the earlier speakers ([`match_previous`])
//! and keeps their rows, so a name or Person link the user set survives (`crate::post`,
//! step 4).
//!
//! The record itself (format, biometric lifetime, per-person deletion with
//! `Store::forget_voiceprints`) lives in [`earshot_store::identities`] and is re-exported
//! here, so the store can erase a person's entries inside its own transaction. The key is
//! the registry's [`MeetingRecord::SpeakerIdentities`], so deleting the meeting erases it.

use earshot_core::capture::TrackId;
use earshot_core::diar::{DiarError, Embedding};
use earshot_core::identity::EntityId;
pub use earshot_store::identities::{
    IDENTITY_RECORD_VERSION, IdentityRecord, RECORD_PREFIX, SpeakerIdentity, StoredIdentities,
    decode, encode,
};

use crate::local::{LocalKey, MeetingRecord};

/// The registry key of `meeting`'s record (the same text as
/// `earshot_store::identities::record_key`, pinned by a test).
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::SpeakerIdentities.key(&[], meeting, &[])
}

/// An earlier speaker that a new cluster may take over.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The track it was found on.
    pub track: TrackId,
    /// Its centroid.
    pub centroid: &'a Embedding,
    /// Whether the user changed its row after the pass (it wins a merge).
    pub edited: bool,
}

/// One-to-one assignment of `new` speakers (track and centroid; `None`: not matchable, such
/// as "Me") to earlier `previous` speakers: for each new speaker, the index of the earlier
/// one it continues, if any.
///
/// A pair is possible only on the same track and with cosine similarity at least
/// `threshold` (the calibrated open-set voiceprint threshold of the exact embedding model:
/// the same "is this the same voice" decision voiceprint naming makes). Pairs are taken
/// greedily: pairs with an edited earlier speaker first, then by similarity, highest first;
/// each speaker in at most one pair. So when two earlier speakers now form one cluster (a
/// merge), the one the user renamed or linked keeps its row and name; between two edited
/// (or two unedited) ones the closer centroid wins. The other earlier speaker is not
/// continued (its row is tombstoned by the caller).
///
/// # Errors
/// [`DiarError::ModelMismatch`] if an embedding of another model is compared (callers drop
/// candidates of another model first).
pub fn match_previous(
    new: &[Option<(TrackId, &Embedding)>],
    previous: &[Candidate<'_>],
    threshold: f32,
) -> Result<Vec<Option<usize>>, DiarError> {
    let mut pairs: Vec<(bool, f32, usize, usize)> = Vec::new();
    for (i, n) in new.iter().enumerate() {
        let Some((track, centroid)) = n else {
            continue;
        };
        if !centroid.is_valid() {
            continue;
        }
        for (j, p) in previous.iter().enumerate() {
            if p.track != *track || !p.centroid.is_valid() {
                continue;
            }
            let similarity = centroid.cosine_similarity(p.centroid)?;
            if similarity >= threshold {
                pairs.push((p.edited, similarity, i, j));
            }
        }
    }
    // Edited first, then by similarity (highest first); ties by index, so the result is
    // deterministic.
    pairs.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });
    let mut out = vec![None; new.len()];
    let mut taken = vec![false; previous.len()];
    for (_, _, i, j) in pairs {
        if out[i].is_none() && !taken[j] {
            out[i] = Some(j);
            taken[j] = true;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::diar::EmbeddingModelId;

    fn emb(v: &[f32]) -> Embedding {
        Embedding::new(EmbeddingModelId::new("m@1"), v.to_vec())
    }

    fn cand(track: TrackId, e: &Embedding, edited: bool) -> Candidate<'_> {
        Candidate {
            track,
            centroid: e,
            edited,
        }
    }

    #[test]
    fn post_identity_record_round_trips_and_refuses_other_versions() {
        let record = IdentityRecord {
            speakers: vec![SpeakerIdentity {
                speaker: EntityId::from_bytes([7; 16]),
                track: TrackId::Reference,
                centroid: emb(&[0.6, 0.8]),
                given_name: "Speaker 1".into(),
                given_person: None,
                given_stamp: None,
                user_edited: true,
                enrolled: false,
                ignored: false,
            }],
        };
        let bytes = encode(&record);
        assert_eq!(bytes[..4], IDENTITY_RECORD_VERSION.to_le_bytes());
        assert_eq!(decode(&bytes), StoredIdentities::Record(record.clone()));
        for version in [0u32, 3] {
            let mut other = bytes.clone();
            other[..4].copy_from_slice(&version.to_le_bytes());
            assert!(
                matches!(decode(&other), StoredIdentities::Unreadable(r) if r.contains(&format!("version {version}"))),
                "{version}"
            );
        }
        // The enrolment flag round-trips; a version 2 record written before it existed reads
        // as not enrolled.
        let mut enrolled = record.clone();
        enrolled.speakers[0].enrolled = true;
        assert_eq!(
            decode(&encode(&enrolled)),
            StoredIdentities::Record(enrolled.clone())
        );
        let mut before_flag = IDENTITY_RECORD_VERSION.to_le_bytes().to_vec();
        let mut json = serde_json::to_value(&enrolled).unwrap();
        json["speakers"][0]
            .as_object_mut()
            .unwrap()
            .remove("enrolled");
        before_flag.extend(serde_json::to_vec(&json).unwrap());
        assert_eq!(
            decode(&before_flag),
            StoredIdentities::Record(record.clone())
        );
        // The ignore flag (T12.19) likewise: it round-trips, and a version 2 record written
        // before it existed reads as not ignored.
        let mut ignored = record.clone();
        ignored.speakers[0].ignored = true;
        assert_eq!(
            decode(&encode(&ignored)),
            StoredIdentities::Record(ignored.clone())
        );
        let mut before_ignore = IDENTITY_RECORD_VERSION.to_le_bytes().to_vec();
        let mut json = serde_json::to_value(&ignored).unwrap();
        json["speakers"][0]
            .as_object_mut()
            .unwrap()
            .remove("ignored");
        before_ignore.extend(serde_json::to_vec(&json).unwrap());
        assert_eq!(
            decode(&before_ignore),
            StoredIdentities::Record(record.clone())
        );
        // A version 1 record (written before the stamp and the edit flag) still reads, with
        // both at their defaults.
        let mut v1 = 1u32.to_le_bytes().to_vec();
        let mut json = serde_json::to_value(&record).unwrap();
        for s in json["speakers"].as_array_mut().unwrap() {
            let s = s.as_object_mut().unwrap();
            s.remove("given_stamp");
            s.remove("user_edited");
            s.remove("enrolled");
        }
        v1.extend(serde_json::to_vec(&json).unwrap());
        let StoredIdentities::Record(old) = decode(&v1) else {
            panic!("{:?}", decode(&v1));
        };
        assert_eq!(old.speakers[0].given_stamp, None);
        assert!(!old.speakers[0].user_edited);
        assert_eq!(old.speakers[0].given_name, "Speaker 1");
        assert!(matches!(
            decode(&bytes[..2]),
            StoredIdentities::Unreadable(_)
        ));
        assert!(matches!(
            decode(&bytes[..bytes.len() - 1]),
            StoredIdentities::Unreadable(_)
        ));
        // The registry key is the store's key (the store erases a person's entries by it).
        let m = EntityId::from_bytes([7; 16]);
        assert_eq!(
            record_key(m).as_str(),
            earshot_store::identities::record_key(m)
        );
        assert_eq!(MeetingRecord::SpeakerIdentities.prefix(), RECORD_PREFIX);
        assert_eq!(crate::local::meeting_of(record_key(m).as_str()), Some(m));
    }

    #[test]
    fn post_identity_match_is_one_to_one_by_similarity_on_the_same_track() {
        let (a, b, c) = (
            emb(&[1.0, 0.0, 0.0]),
            emb(&[0.0, 1.0, 0.0]),
            emb(&[0.0, 0.0, 1.0]),
        );
        let near_a = emb(&[0.9, 0.1, 0.0]);
        let prev = [
            cand(TrackId::Reference, &a, false),
            cand(TrackId::Reference, &b, false),
            cand(TrackId::Mic, &c, false),
        ];
        // b continues b, near_a continues a, a new voice on c's direction but on the other
        // track matches nothing, "Me" (None) is never matched.
        let new = [
            Some((TrackId::Reference, &b)),
            Some((TrackId::Reference, &near_a)),
            Some((TrackId::Reference, &c)),
            None,
        ];
        assert_eq!(
            match_previous(&new, &prev, 0.5).unwrap(),
            [Some(1), Some(0), None, None]
        );
        // Two new clusters close to one earlier speaker: only the closer continues it.
        let new = [
            Some((TrackId::Reference, &near_a)),
            Some((TrackId::Reference, &a)),
        ];
        assert_eq!(match_previous(&new, &prev, 0.5).unwrap(), [None, Some(0)]);
        // Below the threshold: no carry-over.
        assert_eq!(
            match_previous(&[Some((TrackId::Reference, &near_a))], &prev, 0.999).unwrap(),
            [None]
        );
    }

    #[test]
    fn post_identity_merge_prefers_the_edited_speaker_then_the_closer_one() {
        let (a, b) = (emb(&[1.0, 0.0]), emb(&[0.0, 1.0]));
        // One cluster now, closer to a than to b.
        let merged = [Some((TrackId::Reference, &emb(&[0.8, 0.6])))];
        let unedited = [
            cand(TrackId::Reference, &a, false),
            cand(TrackId::Reference, &b, false),
        ];
        assert_eq!(match_previous(&merged, &unedited, 0.5).unwrap(), [Some(0)]);
        let b_renamed = [
            cand(TrackId::Reference, &a, false),
            cand(TrackId::Reference, &b, true),
        ];
        assert_eq!(match_previous(&merged, &b_renamed, 0.5).unwrap(), [Some(1)]);
        let both = [
            cand(TrackId::Reference, &a, true),
            cand(TrackId::Reference, &b, true),
        ];
        assert_eq!(match_previous(&merged, &both, 0.5).unwrap(), [Some(0)]);
    }

    #[test]
    fn post_identity_match_refuses_another_model_and_skips_invalid_centroids() {
        let a = emb(&[1.0, 0.0]);
        let other = Embedding::new(EmbeddingModelId::new("m@2"), vec![1.0, 0.0]);
        assert!(matches!(
            match_previous(
                &[Some((TrackId::Mic, &other))],
                &[cand(TrackId::Mic, &a, false)],
                0.5
            ),
            Err(DiarError::ModelMismatch { .. })
        ));
        let zero = emb(&[0.0, 0.0]);
        assert_eq!(
            match_previous(
                &[Some((TrackId::Mic, &zero)), Some((TrackId::Mic, &a))],
                &[
                    cand(TrackId::Mic, &zero, true),
                    cand(TrackId::Mic, &a, false)
                ],
                0.5
            )
            .unwrap(),
            [None, Some(1)]
        );
    }
}
