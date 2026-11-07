//! Track policies (PLAN.md 3.3): how per-track diarisations become the meeting's speakers.
//!
//! - **Two-track** (Windows calls, [`two_track`]): the mic track ([`TrackId::Mic`]) is the
//!   single speaker "Me" ([`SpeakerRole::Me`]) unless its clustering found more than one local
//!   voice (a shared room); then each mic cluster stays a separate [`SpeakerRole::Local`]
//!   speaker, and which of them is "Me" is decided later by voiceprint matching (T3.4). The
//!   loopback track ([`TrackId::Reference`]) is diarised into [`SpeakerRole::Remote`]
//!   speakers.
//! - **In-person** ([`in_person`], Android or the Windows in-room mode): one track, fully
//!   diarised; every cluster is a [`SpeakerRole::Room`] speaker (none is assumed to be "Me").
//!
//! Speakers are numbered mic speakers first, then loopback speakers, each in their track's
//! order (first turn); turns are sorted by start, then speaker.

use std::time::Duration;

use super::cluster::normalized;
use super::pipeline::Diarization;
use super::types::{DiarError, Embedding};
use crate::asr::MeetingTime;
use crate::capture::TrackId;

/// What a meeting speaker is, before voiceprint naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerRole {
    /// The user: all mic speech under the two-track policy with one local voice.
    Me,
    /// One of several voices on the mic track (shared room); "Me" is one of them.
    Local,
    /// A far-end speaker from the loopback track.
    Remote,
    /// A speaker of an in-person (single-track) recording.
    Room,
}

/// One speaker of the meeting.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingSpeaker {
    /// Role under the policy.
    pub role: SpeakerRole,
    /// Track the speaker was found on.
    pub track: TrackId,
    /// Unit-length centroid of the speaker's cluster.
    pub centroid: Embedding,
    /// Total speech.
    pub speech: Duration,
}

/// One stretch of speech of one meeting speaker. Turns may overlap (across tracks, or
/// overlapped speech within a track).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingTurn {
    /// Index into [`MeetingDiarization::speakers`].
    pub speaker: usize,
    /// Track the turn comes from.
    pub track: TrackId,
    /// Start (meeting time).
    pub start: MeetingTime,
    /// End (meeting time, exclusive).
    pub end: MeetingTime,
}

/// The meeting's speakers and turns after a track policy.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingDiarization {
    /// Speakers: mic track first, then loopback.
    pub speakers: Vec<MeetingSpeaker>,
    /// Turns sorted by start, then speaker.
    pub turns: Vec<MeetingTurn>,
}

impl MeetingDiarization {
    /// Index of the "Me" speaker, if the policy assigned one.
    #[must_use]
    pub fn me(&self) -> Option<usize> {
        self.speakers.iter().position(|s| s.role == SpeakerRole::Me)
    }
}

/// Appends `d`'s speakers with `role` and returns their new indices.
fn append(
    out: &mut MeetingDiarization,
    d: &Diarization,
    track: TrackId,
    role: SpeakerRole,
) -> Vec<usize> {
    let base = out.speakers.len();
    for s in &d.speakers {
        out.speakers.push(MeetingSpeaker {
            role,
            track,
            centroid: s.centroid.clone(),
            speech: s.speech,
        });
    }
    for t in &d.turns {
        out.turns.push(MeetingTurn {
            speaker: base + t.speaker,
            track,
            start: t.start,
            end: t.end,
        });
    }
    (base..out.speakers.len()).collect()
}

fn sort_turns(turns: &mut [MeetingTurn]) {
    turns.sort_by_key(|t| (t.start, t.speaker, t.end));
}

/// Turns the single mic speaker of `d` into "Me": its unit-length centroid, and its turns
/// with overlapping or touching ones merged (overlap reconstruction can emit overlapping turns
/// of one speaker), so Me's speech is never double-counted.
///
/// # Errors
/// [`DiarError::Config`] unless `d` has exactly one speaker, [`DiarError::ModelMismatch`] if
/// its centroid is not of `d`'s embedding model, [`DiarError::Shape`] for an invalid centroid.
fn collapse_to_me(d: &Diarization) -> Result<(MeetingSpeaker, Vec<MeetingTurn>), DiarError> {
    let [s] = d.speakers.as_slice() else {
        return Err(DiarError::Config(format!(
            "\"Me\" needs exactly one mic speaker, got {}",
            d.speakers.len()
        )));
    };
    if s.centroid.model != d.embedding_model {
        return Err(DiarError::ModelMismatch {
            left: d.embedding_model.clone(),
            right: s.centroid.model.clone(),
        });
    }
    let centroid = normalized(&s.centroid.vector)
        .ok_or_else(|| DiarError::Shape("invalid speaker centroid".to_owned()))?;
    let mut spans: Vec<(MeetingTime, MeetingTime)> =
        d.turns.iter().map(|t| (t.start, t.end)).collect();
    spans.sort();
    let mut merged: Vec<(MeetingTime, MeetingTime)> = Vec::new();
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let speech = merged
        .iter()
        .map(|(s, e)| Duration::from_millis(e.as_millis() - s.as_millis()))
        .sum();
    let turns = merged
        .into_iter()
        .map(|(start, end)| MeetingTurn {
            speaker: 0,
            track: TrackId::Mic,
            start,
            end,
        })
        .collect();
    Ok((
        MeetingSpeaker {
            role: SpeakerRole::Me,
            track: TrackId::Mic,
            centroid: Embedding::new(d.embedding_model.clone(), centroid),
            speech,
        },
        turns,
    ))
}

/// Two-track policy (Windows calls): `mic` is "Me" unless it has more than one speaker;
/// `loopback` speakers are remote.
///
/// # Errors
/// [`DiarError::ModelMismatch`] if the two tracks were embedded with different models (their
/// speakers could never be compared), [`DiarError::Shape`] for invalid centroids.
pub fn two_track(
    mic: &Diarization,
    loopback: &Diarization,
) -> Result<MeetingDiarization, DiarError> {
    let have_both = !mic.speakers.is_empty() && !loopback.speakers.is_empty();
    if have_both && mic.embedding_model != loopback.embedding_model {
        return Err(DiarError::ModelMismatch {
            left: mic.embedding_model.clone(),
            right: loopback.embedding_model.clone(),
        });
    }
    let mut out = MeetingDiarization {
        speakers: Vec::new(),
        turns: Vec::new(),
    };
    match mic.speakers.len() {
        0 => {}
        1 => {
            let (me, turns) = collapse_to_me(mic)?;
            out.speakers.push(me);
            out.turns.extend(turns);
        }
        _ => {
            append(&mut out, mic, TrackId::Mic, SpeakerRole::Local);
        }
    }
    append(&mut out, loopback, TrackId::Reference, SpeakerRole::Remote);
    sort_turns(&mut out.turns);
    Ok(out)
}

/// In-person policy: one track, every speaker kept, on the `track` it was recorded on
/// (normally the mic; the loopback when only it is available, for example a
/// re-transcription whose mic recording was deleted).
#[must_use]
pub fn in_person(room: &Diarization, track: TrackId) -> MeetingDiarization {
    let mut out = MeetingDiarization {
        speakers: Vec::new(),
        turns: Vec::new(),
    };
    append(&mut out, room, track, SpeakerRole::Room);
    sort_turns(&mut out.turns);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::{DiarSpeaker, EmbeddingModelId, SpeakerTurn};

    fn model() -> EmbeddingModelId {
        EmbeddingModelId::new("test-model@1")
    }

    fn speaker(v: Vec<f32>, secs: u64) -> DiarSpeaker {
        DiarSpeaker {
            centroid: Embedding::new(model(), v),
            speech: Duration::from_secs(secs),
        }
    }

    fn turn(speaker: usize, s: u64, e: u64) -> SpeakerTurn {
        SpeakerTurn {
            speaker,
            start: MeetingTime::from_millis(s),
            end: MeetingTime::from_millis(e),
        }
    }

    fn diar(speakers: Vec<DiarSpeaker>, turns: Vec<SpeakerTurn>) -> Diarization {
        Diarization {
            speakers,
            turns,
            embedding_model: model(),
        }
    }

    fn empty() -> Diarization {
        diar(Vec::new(), Vec::new())
    }

    #[test]
    fn single_mic_voice_is_me_and_loopback_speakers_are_remote() {
        let mic = diar(
            vec![speaker(vec![1.0, 0.0, 0.0], 4)],
            vec![turn(0, 0, 1000), turn(0, 5000, 8000)],
        );
        let lb = diar(
            vec![
                speaker(vec![0.0, 1.0, 0.0], 3),
                speaker(vec![0.0, 0.0, 1.0], 2),
            ],
            vec![turn(0, 1000, 4000), turn(1, 8000, 10_000)],
        );
        let m = two_track(&mic, &lb).unwrap();
        let roles: Vec<_> = m.speakers.iter().map(|s| (s.role, s.track)).collect();
        assert_eq!(
            roles,
            [
                (SpeakerRole::Me, TrackId::Mic),
                (SpeakerRole::Remote, TrackId::Reference),
                (SpeakerRole::Remote, TrackId::Reference),
            ]
        );
        assert_eq!(m.me(), Some(0));
        let t: Vec<_> = m
            .turns
            .iter()
            .map(|t| (t.speaker, t.track, t.start.as_millis(), t.end.as_millis()))
            .collect();
        assert_eq!(
            t,
            [
                (0, TrackId::Mic, 0, 1000),
                (1, TrackId::Reference, 1000, 4000),
                (0, TrackId::Mic, 5000, 8000),
                (2, TrackId::Reference, 8000, 10_000),
            ]
        );
        assert_eq!(m.speakers[0].speech, Duration::from_secs(4));
        assert_eq!(m.speakers[0].centroid.vector, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn several_mic_voices_stay_separate_local_speakers_without_a_me() {
        let mic = diar(
            vec![speaker(vec![1.0, 0.0], 10), speaker(vec![0.0, 1.0], 6)],
            vec![turn(0, 0, 3000), turn(1, 2000, 5000)],
        );
        let m = two_track(&mic, &empty()).unwrap();
        assert_eq!(m.speakers.len(), 2);
        assert!(m.speakers.iter().all(|s| s.role == SpeakerRole::Local));
        assert!(m.speakers.iter().all(|s| s.track == TrackId::Mic));
        assert_eq!(
            m.me(),
            None,
            "which local voice is Me is left to voiceprints"
        );
        assert_eq!(m.turns.len(), 2);
        assert_eq!(
            (m.turns[1].speaker, m.turns[1].start.as_millis()),
            (1, 2000)
        );
    }

    #[test]
    fn me_turns_are_the_union_of_overlapping_mic_turns() {
        // One mic speaker, but overlapping turns (overlap reconstruction) must not
        // double-count Me.
        let mic = diar(
            vec![speaker(vec![0.6, 0.8], 5)],
            vec![
                turn(0, 0, 2000),
                turn(0, 1500, 3000),
                turn(0, 3000, 3500),
                turn(0, 6000, 7000),
            ],
        );
        let m = two_track(&mic, &empty()).unwrap();
        let t: Vec<_> = m
            .turns
            .iter()
            .map(|t| (t.start.as_millis(), t.end.as_millis()))
            .collect();
        assert_eq!(t, [(0, 3500), (6000, 7000)]);
        assert_eq!(m.speakers[0].speech, Duration::from_millis(4500));
        let c = &m.speakers[0].centroid.vector;
        assert!((c[0] - 0.6).abs() < 1e-6 && (c[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn collapse_to_me_refuses_anything_but_one_valid_speaker() {
        let two = diar(
            vec![speaker(vec![1.0, 0.0], 1), speaker(vec![0.0, 1.0], 1)],
            vec![turn(0, 0, 1000)],
        );
        assert!(matches!(collapse_to_me(&two), Err(DiarError::Config(_))));
        assert!(matches!(
            collapse_to_me(&empty()),
            Err(DiarError::Config(_))
        ));
        let zero = diar(vec![speaker(vec![0.0, 0.0], 1)], vec![turn(0, 0, 1000)]);
        assert!(matches!(collapse_to_me(&zero), Err(DiarError::Shape(_))));
        let mut other = diar(vec![speaker(vec![1.0, 0.0], 1)], vec![turn(0, 0, 1000)]);
        other.speakers[0].centroid.model = EmbeddingModelId::new("other@2");
        assert!(matches!(
            collapse_to_me(&other),
            Err(DiarError::ModelMismatch { .. })
        ));
        // A non-unit centroid comes out unit length.
        let scaled = diar(vec![speaker(vec![3.0, 4.0], 1)], vec![turn(0, 0, 1000)]);
        let (me, _) = collapse_to_me(&scaled).unwrap();
        assert_eq!(me.centroid.vector, [0.6, 0.8]);
    }

    #[test]
    fn empty_tracks_give_no_speakers() {
        let m = two_track(&empty(), &empty()).unwrap();
        assert!(m.speakers.is_empty() && m.turns.is_empty());
        let lb = diar(vec![speaker(vec![1.0, 0.0], 2)], vec![turn(0, 0, 2000)]);
        let m = two_track(&empty(), &lb).unwrap();
        assert_eq!(m.me(), None);
        assert_eq!(m.speakers[0].role, SpeakerRole::Remote);
        assert_eq!(m.turns[0].speaker, 0);
    }

    #[test]
    fn tracks_embedded_with_different_models_are_refused() {
        let mic = diar(vec![speaker(vec![1.0, 0.0], 2)], vec![turn(0, 0, 1000)]);
        let mut lb = diar(vec![speaker(vec![1.0, 0.0], 2)], vec![turn(0, 0, 1000)]);
        lb.embedding_model = EmbeddingModelId::new("other@2");
        lb.speakers[0].centroid.model = lb.embedding_model.clone();
        assert!(matches!(
            two_track(&mic, &lb),
            Err(DiarError::ModelMismatch { .. })
        ));
    }

    /// Runs the real pipeline over a synthetic scene with the fakes (ground truth from
    /// `scene_audio`).
    fn pipeline(turns: &[crate::diar::SceneTurn], total_ms: u64) -> Diarization {
        use crate::asr::CancelToken;
        use crate::diar::{
            DiarizationConfig, Diarizer, FakeEmbedder, FakeSegmenter, MAX_SCENE_SPEAKERS,
            scene_audio,
        };
        let dim = 16;
        let centroids: Vec<Vec<f32>> = (0..MAX_SCENE_SPEAKERS)
            .map(|k| (0..dim).map(|i| if i == k { 1.0 } else { 0.0 }).collect())
            .collect();
        let embedder = FakeEmbedder::new("fake-emb@1", centroids, 0.05, 11);
        let mut d = Diarizer::new(
            FakeSegmenter::segmentation_3_0(),
            embedder,
            DiarizationConfig::default(),
        )
        .unwrap();
        d.diarize(
            &scene_audio(turns, total_ms),
            MeetingTime::ZERO,
            &CancelToken::new(),
        )
        .unwrap()
    }

    #[test]
    fn two_track_policy_over_the_pipeline_one_local_voice_and_two_remote() {
        use crate::diar::SceneTurn;
        let mic = pipeline(
            &[
                SceneTurn::new(0, 0, 6_000),
                SceneTurn::new(0, 20_000, 30_000),
            ],
            40_000,
        );
        let lb = pipeline(
            &[
                SceneTurn::new(1, 6_000, 14_000),
                SceneTurn::new(2, 14_000, 20_000),
                SceneTurn::new(1, 30_000, 38_000),
            ],
            40_000,
        );
        let m = two_track(&mic, &lb).unwrap();
        let roles: Vec<_> = m.speakers.iter().map(|s| s.role).collect();
        assert_eq!(
            roles,
            [SpeakerRole::Me, SpeakerRole::Remote, SpeakerRole::Remote]
        );
        let me_turns: Vec<_> = m.turns.iter().filter(|t| t.speaker == 0).collect();
        assert!(me_turns.iter().all(|t| t.track == TrackId::Mic));
        let me_ms: u64 = me_turns
            .iter()
            .map(|t| t.end.as_millis() - t.start.as_millis())
            .sum();
        assert!(
            (15_000..=17_000).contains(&me_ms),
            "Me speaks about 16 s, got {me_ms} ms"
        );
    }

    #[test]
    fn two_track_policy_over_the_pipeline_shared_room_keeps_local_voices() {
        use crate::diar::SceneTurn;
        let mic = pipeline(
            &[
                SceneTurn::new(0, 0, 8_000),
                SceneTurn::new(3, 8_000, 16_000),
                SceneTurn::new(0, 16_000, 24_000),
                SceneTurn::new(3, 24_000, 32_000),
            ],
            34_000,
        );
        let m = two_track(&mic, &pipeline(&[SceneTurn::new(1, 0, 5_000)], 34_000)).unwrap();
        let roles: Vec<_> = m.speakers.iter().map(|s| (s.role, s.track)).collect();
        assert_eq!(
            roles,
            [
                (SpeakerRole::Local, TrackId::Mic),
                (SpeakerRole::Local, TrackId::Mic),
                (SpeakerRole::Remote, TrackId::Reference),
            ]
        );
        assert_eq!(m.me(), None);
    }

    #[test]
    fn in_person_keeps_every_speaker_as_room_speakers() {
        let room = diar(
            vec![
                speaker(vec![1.0, 0.0], 3),
                speaker(vec![0.0, 1.0], 3),
                speaker(vec![0.7, 0.7], 1),
            ],
            vec![turn(1, 500, 900), turn(0, 0, 1000), turn(2, 2000, 2500)],
        );
        let m = in_person(&room, TrackId::Mic);
        assert_eq!(m.speakers.len(), 3);
        assert!(m.speakers.iter().all(|s| s.role == SpeakerRole::Room));
        assert!(m.speakers.iter().all(|s| s.track == TrackId::Mic));
        assert!(m.turns.iter().all(|t| t.track == TrackId::Mic));
        // The track given is the track reported (a loopback-only meeting).
        let lb = in_person(&room, TrackId::Reference);
        assert!(lb.speakers.iter().all(|s| s.track == TrackId::Reference));
        assert!(lb.turns.iter().all(|t| t.track == TrackId::Reference));
        assert_eq!(m.me(), None);
        let order: Vec<_> = m.turns.iter().map(|t| t.speaker).collect();
        assert_eq!(order, [0, 1, 2], "sorted by start");
    }
}
