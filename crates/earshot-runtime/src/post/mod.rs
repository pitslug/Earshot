//! Post-meeting processing (PLAN.md 3.3 "On stop", T6.4b): final pass -> diarisation per
//! track -> track policy -> voiceprint naming -> labelled final transcript.
//!
//! [`load_post_input`] turns a live session's [`FinalPassRequest`] into a [`PostInput`]
//! (the recorded tracks, read back from FLAC at their stored rate). [`PostProcessor::run`]
//! then:
//!
//! 1. **Final pass**: each track is transcribed with the request's options. With
//!    [`PostProcessor::with_pieces`] (always in production) it is decoded in VAD speech
//!    pieces of at most one whisper window with a repetition check
//!    ([`earshot_core::asr::transcribe_in_pieces`], T6.16); pieces dropped as loops are a
//!    [`PostWarning::RepetitionDropped`]. Without it one call decodes the whole track, which
//!    lets whisper carry a repetition loop across a long recording.
//!
//!    **Reusing the live speech stretches (T6.13c, D20).** A track whose live session ran
//!    speech stretches has a record of what they decoded ([`crate::stretches`]: the raw
//!    pieces, before the live echo filter, and the frontier they cover). The final pass
//!    takes those pieces instead of transcribing them again and decodes only the plan's
//!    pieces after the frontier
//!    ([`earshot_core::asr::transcribe_rest_in_pieces`]), when, and only when, it is the
//!    same transcription: the record's model is exactly the final-pass model
//!    ([`earshot_core::asr::BackendInfo::model`]), its decoding options are the request's
//!    (a stretch track's request carries the stretches' own options, greedy during the
//!    meeting: `LiveTrack::final_pass`), its piece settings and VAD (its
//!    [`Vad::id`] and frame length) are the final pass's, and the record is consistent
//!    with the recording. Otherwise, without a record (the live chunker, an older
//!    meeting), or without pieces, the track is transcribed whole as before; a record this
//!    build cannot read (another version, damaged) is a
//!    [`PostWarning::StretchRecordUnreadable`] and the track is transcribed whole, never an
//!    error. When every processed track is taken whole from its stretches (nothing is
//!    decoded here), the pass backend ([`PostOutcome::backend`]) is the live backend that
//!    decoded them, not the idle final-pass transcriber's. Fed the audio the stretches heard, the result equals the
//!    whole-track pass; on the stored 16-bit recording a later piece is decoded from the
//!    frontier on (core `asr::stretch` docs). Pieces the stretches dropped as loops are
//!    reported as [`PostWarning::RepetitionDropped`] like the final pass's own, and
//!    [`PostOutcome::stretches`] says per track what was reused and why not. Echo cleaning
//!    below always starts from these raw words, so the final transcript never depends on
//!    what the live echo filter removed. With [`PostConfig::reuse_live`] off neither the
//!    stretches nor the speaker windows (step 2) are taken ([`WHOLE_PASS_REQUESTED`]): the
//!    whole-meeting pass with the same models, for comparing the two (T6.13).
//! 2. **Diarisation** of each track with the [`Diarizer`].
//!
//!    **Reusing the live speaker windows (T6.13d2).** A track whose live worker computed
//!    speaker windows during the meeting has a record of them ([`crate::speakers`]). The
//!    final pass finishes from them ([`Diarizer::diarize_from`]: the remaining windows,
//!    then clustering as always) when, and only when, nothing that decides a window differs:
//!    the record's window settings and models are the final-pass diarizer's
//!    ([`Diarizer::window_params`]: segmentation model and file hash, embedding model, window
//!    grid, onset, overlap rule, batch size) and its speaker models ran with the same ONNX
//!    Runtime thread count ([`PostProcessor::with_speaker_threads`]; CAM++ output changes
//!    with it). The live worker computed the windows from the 16-bit samples the FLAC
//!    stores, so the result equals diarising the recording whole. Otherwise, without a
//!    record, or when the record does not fit the recording, the track is diarised whole;
//!    [`PostOutcome::speaker_windows`] says per track what was reused and why not. A record
//!    this build cannot read is a [`PostWarning::SpeakerRecordUnreadable`], never an error.
//!    Both live records of every processed track are deleted in the final pass's write
//!    transaction (step 5), whatever was reused; a failed or cancelled pass keeps them.
//!    Then **echo cleaning** of the mic ("Me") transcript, only with the live session's echo
//!    lock stretches ([`PostInput::echo_locks`], each with its delay, short gaps at the
//!    same delay bridged by [`crate::echo::cleaning_stretches`]; outside them nothing
//!    is removed) and both tracks processed: the text rule
//!    (`asr::dedupe_echo`) and the speaker check (`diar::speaker_echo_spans` on
//!    `diar::echo_span_pieces`), so the stored final transcript never attributes far-end
//!    speech to "Me". A mic cluster whose words were all removed is an echo cluster and
//!    is dropped before the policy (never a "Local" voice or "Speaker N"). So is (T6.23a;
//!    a rule defined after seeing the a private recording data, with nothing numeric tuned) a
//!    mic cluster for which all three hold: (a) echo cleaning removed more of its words
//!    than it kept (strictly more than half, a midpoint); (b) every word it kept lies
//!    inside a cleaning stretch; (c) its centroid's cosine with some loopback speaker is at
//!    or above the embedding model's existing voiceprint calibration. This holds even when
//!    it passed the speaker check's user test (a cluster can pass it by "talking alone" in
//!    residual sound while the far end is silent, and its spans are then judged against
//!    its own centroid). The rule never removes every mic voice that still has words
//!    ([`drop_echo_clusters`]) and, as part of the speaker check, applies only when that
//!    check ran. Words a rule-2 echo cluster kept are removed and counted
//!    ([`EchoReport::cluster_removed`]; decided 2026-10-26 by the coordinator, pending
//!    The owner), never moved to another mic speaker. Known edge case: a user whose own voice
//!    comes back on the loopback matches a loopback speaker, so with a second mic voice
//!    present and most of the user's words removed as echo, the user's cluster would be
//!    dropped. The rule decides whether a cluster is a voice; it never names "Me" by
//!    amount of speech. "Me" comes from
//!    the policy (a single mic voice) or the user's voiceprint (step 4); only when neither
//!    named one does the speaker check, and only if it discriminated (exactly one remaining
//!    mic voice passes its user test, the others sound like the far end): never by amount
//!    of speech, and nothing is enrolled from that choice. An
//!    embedding model without a calibrated margin gets no speaker check, reported as
//!    [`PostWarning::NoSpeakerEchoCheck`]; nothing is removed without a lock.
//! 3. **Track policy**, chosen by the meeting's recorded tracks: [`two_track`] for a
//!    meeting with a mic and a loopback ("Me" on the mic unless it holds several voices,
//!    remote speakers on the loopback), also when only one of them is processed now;
//!    [`in_person`] for a single-track meeting, on that track.
//! 4. **Naming**: stored voiceprints are loaded once and every speaker's centroid is
//!    matched against them ([`match_embedding_with_report`]). A known person gives the
//!    speaker their name. Among several mic voices ([`SpeakerRole::Local`], or the
//!    [`SpeakerRole::Room`] voices of an in-person meeting recorded on the mic, T6.27), the
//!    one that best matches the user becomes "Me" (the promotion is made in the diarisation
//!    result, so enrolment follows it); other matches to the user are ignored, and a
//!    far-end speaker is never "Me". In person, "Me" therefore comes only from the
//!    voiceprint, never from the amount of speech. A "Me" speaker is linked to the user's person
//!    ([`me_person`]). Unknown speakers are "Speaker 1", "Speaker 2", ... in meeting
//!    order (continuing after any kept "Speaker N").
//!
//!    **Carrying speakers across runs (T6.25).** Every run keeps the centroid of each
//!    speaker row it wrote ("Me" excepted) with the name and person it gave the row
//!    ([`crate::identities`], device-local, biometric). A re-run matches its new speakers
//!    (not "Me") one to one to the earlier speakers it replaces, on the same track, by
//!    cosine similarity at or above the calibrated voiceprint threshold of the exact
//!    embedding model ([`match_previous`]: greedy, a speaker the user edited first, then the
//!    most similar pair). A matched speaker keeps its row: when the user changed its name or
//!    Person link (the row differs from what the pass gave it) both are kept as they are and
//!    only the role follows the policy; otherwise this run names it as above (so automatic
//!    numbers stay dense). When two earlier speakers now form one cluster, the edited one
//!    wins, else the closer one, and the other row becomes a tombstone. A carried name of the
//!    form "Speaker N" is never given to a new speaker. "Me" always stays the Me row (below).
//!    The one Me row that takes part is a voice the user marked as themselves in an in-room
//!    meeting ([`crate::edit::mark_me`], T9.12): it keeps its entry, and while this run named
//!    no "Me" from the voiceprint it continues like any edit and stays the Me row; its
//!    diarisation speaker stays a room voice, so it is never enrolled from. A voiceprint "Me"
//!    takes the one Me row over; an unmatched voice loses the mark with its row.
//!    Earlier speakers found with another embedding model, or an identity record this build
//!    cannot read, are not carried over ([`PostWarning::SpeakerNamesNotCarried`]). This
//!    holds for the whole-meeting pass ([`PostConfig::reuse_live`] off) too.
//!
//!    **Voices without words (T7.3b).** The segments are labelled with their speakers
//!    before any row is written; a speaker no segment part goes to (heard by diarisation,
//!    but nothing it said was transcribed) gets no row, is not matched to an earlier
//!    speaker, uses no "Speaker N" number and is reported as
//!    [`PostWarning::VoiceWithoutWords`]. An earlier row it would have continued becomes a
//!    tombstone like any replaced row. "Me" enrolment still follows the diarisation result.
//! 5. **Persistence**, in one transaction (a failure writes nothing): for the tracks
//!    processed, their earlier final segments become tombstones and the new labelled
//!    segments ([`TranscriptPass::Final`], split where a segment changes speaker) are
//!    written; speakers this run replaces and does not continue (step 4) become
//!    tombstones, and the identity record is rewritten in the same transaction.
//!    Tracks not processed keep their final segments and speakers, so a re-run over fewer
//!    tracks never shrinks another track's transcript, and an existing "Me" speaker row
//!    (with any name the user gave it) is reused rather than duplicated.
//!
//!    **Edited text (T12.16).** A final segment whose text the user edited
//!    (`SegmentRow::text_edited_at`) is kept on every run, with its id, times, text and
//!    speaker: the user's words win over a new decode. The new decode gives way to it by
//!    [`drop_words_under_edits`] (every new word on the same track whose time overlaps the
//!    edited segment's span is left out) and the run reports
//!    [`PostWarning::EditedLinesKept`]. The edited segment's speaker row is never made a
//!    tombstone, though a new voice may still continue it (step 4); its "Speaker N" number
//!    is not given to anyone else. Only when [`PostConfig::enroll_me`] is set, the
//!    user's voiceprint is updated from the "Me" centroid ([`enroll_me`]). The live
//!    transcript is kept.
//!
//! An input without tracks is refused ([`RuntimeError::NoTracks`]).
//!
//! # Calibration
//!
//! Both thresholds depend on the embedding model and are keyed by exact
//! [`EmbeddingModelId`]: an override in [`PostConfig`], else the dev-data calibration in
//! core ([`calibrated_cluster_threshold`], [`earshot_core::voiceprint::calibrated_threshold`]).
//! pyannote's clustering default 0.7046 is for WeSpeaker embeddings and is never used as a
//! fallback (AGENTS.md). A model without either is an
//! error when the processor is built ([`RuntimeError::Uncalibrated`]), never a silent
//! fallback.
//!
//! Model calls block; the processor runs on the caller's thread or its own
//! ([`PostProcessor::spawn`]). The store is locked only to read rows (FLAC is decoded
//! after the lock is released) and for the final transaction.

use std::collections::BTreeSet;
use std::fmt;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use earshot_core::asr::{
    AsrError, BackendInfo, CancelToken, MeetingTime, PieceConfig, Segment, TranscribeOptions,
    Transcriber, Transcription, Vad, Word, transcribe_in_pieces, transcribe_rest_in_pieces,
};
use earshot_core::audio::{MonoBuffer, SampleRate};
use earshot_core::capture::TrackId;
use earshot_core::diar::DiarError;
use earshot_core::diar::cluster::calibrated_cluster_threshold;
use earshot_core::diar::{
    Diarization, DiarizationConfig, Diarizer, EmbeddingModelId, MeetingDiarization, MeetingSpeaker,
    SpanEmbedding, SpeakerEchoConfig, SpeakerEmbedder, SpeakerRole, SpeakerSegmenter, SpeakerTurn,
    in_person, label_segment_speakers, label_segments, remove_speaker_echo, speaker_echo_spans,
    two_track,
};
use earshot_core::diar::{echo_span_pieces, me_speakers};
use earshot_core::identity::EntityId;
use earshot_core::model::{
    AudioFileRepo, MeetingRepo, SegmentRow, Speaker, Stamp, Track, TranscriptPass,
};
use earshot_core::pipeline::FinalPassRequest;
use earshot_core::voiceprint::{
    EnrolmentConfig, InMemoryVoiceprintRepo, MatchOutcome, VoiceprintError, VoiceprintRepo,
    calibrated_threshold, enroll, enroll_me, match_embedding_with_report, me_person,
};
use earshot_store::read_flac;
use rand_core::Rng;

use crate::echo::{
    EchoLock, EchoReport, bridged_gaps, cleaning_stretches, record_echo_report, text_echo_mask,
};
use crate::identities::{
    Candidate, IdentityRecord, SpeakerIdentity, StoredIdentities, match_previous,
};
use crate::speakers::{SpeakerRecord, StoredSpeakers, speaker_record};
use crate::stretches::{StoredStretches, StretchRecord, stretch_record};
use crate::{BoxRng, RuntimeError, Services};

#[cfg(test)]
pub(crate) mod reuse_tests;
#[cfg(test)]
pub(crate) mod tests;

/// Which threshold a model lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Calibration {
    /// The diarisation clustering threshold ([`earshot_core::diar::ClusteringConfig`]).
    Clustering,
    /// The open-set voiceprint match threshold.
    Voiceprint,
}

impl fmt::Display for Calibration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Clustering => "clustering",
            Self::Voiceprint => "voiceprint",
        })
    }
}

/// Thresholds per embedding model (exact [`EmbeddingModelId`] match).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelThresholds(Vec<(EmbeddingModelId, f32)>);

impl ModelThresholds {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// The table with `threshold` for `model` (replacing an earlier entry).
    #[must_use]
    pub fn with(mut self, model: EmbeddingModelId, threshold: f32) -> Self {
        self.0.retain(|(m, _)| *m != model);
        self.0.push((model, threshold));
        self
    }

    /// The threshold for `model`, if listed.
    #[must_use]
    pub fn get(&self, model: &EmbeddingModelId) -> Option<f32> {
        self.0.iter().find(|(m, _)| m == model).map(|(_, t)| *t)
    }
}

/// Post-meeting settings.
#[derive(Debug, Clone, PartialEq)]
pub struct PostConfig {
    /// Diarisation settings; `clustering.threshold` is replaced by the calibrated value.
    pub diarization: DiarizationConfig,
    /// Clustering thresholds that override the core calibration (tests, a newly
    /// calibrated model). Default: empty.
    pub clustering_thresholds: ModelThresholds,
    /// Voiceprint match thresholds that override the core calibration (tests, a newly
    /// calibrated model). Default: empty.
    pub voiceprint_thresholds: ModelThresholds,
    /// Store the user's voiceprint from the "Me" speaker. Default off: voiceprints are
    /// biometric data (PLAN.md 3.9), so the host turns this on after the user agrees.
    pub enroll_me: bool,
    /// Enrolment settings (minimum speech for a new voiceprint).
    pub enrolment: EnrolmentConfig,
    /// Take the live speech stretches and speaker windows where they are the same work
    /// (module docs, steps 1 and 2). Default on; off runs the whole-meeting pass with the
    /// same models (the fallback), for comparing the two (T6.13, the CLI's `--whole-pass`).
    /// The live records are deleted either way.
    pub reuse_live: bool,
}

/// The reason a track was transcribed or diarised whole when [`PostConfig::reuse_live`] is
/// off.
pub const WHOLE_PASS_REQUESTED: &str = "the whole-meeting pass was requested";

impl Default for PostConfig {
    fn default() -> Self {
        Self {
            diarization: DiarizationConfig::default(),
            clustering_thresholds: ModelThresholds::new(),
            voiceprint_thresholds: ModelThresholds::new(),
            enroll_me: false,
            enrolment: EnrolmentConfig::default(),
            reuse_live: true,
        }
    }
}

impl PostConfig {
    /// The voiceprint threshold for `model`: an override, else the core calibration.
    ///
    /// # Errors
    /// [`RuntimeError::Uncalibrated`] if neither has one.
    pub fn voiceprint_threshold(&self, model: &EmbeddingModelId) -> Result<f32, RuntimeError> {
        if let Some(t) = self.voiceprint_thresholds.get(model) {
            return Ok(t);
        }
        calibrated_threshold(model).map_err(|e| match e {
            VoiceprintError::NoThreshold(model) => RuntimeError::Uncalibrated {
                purpose: Calibration::Voiceprint,
                model,
            },
            other => RuntimeError::Voiceprint(other),
        })
    }

    /// The clustering threshold for `model`: an override, else the core calibration
    /// ([`calibrated_cluster_threshold`]).
    ///
    /// # Errors
    /// [`RuntimeError::Uncalibrated`] if neither has one.
    pub fn clustering_threshold(&self, model: &EmbeddingModelId) -> Result<f32, RuntimeError> {
        self.clustering_thresholds
            .get(model)
            .or_else(|| calibrated_cluster_threshold(model))
            .ok_or_else(|| RuntimeError::Uncalibrated {
                purpose: Calibration::Clustering,
                model: model.clone(),
            })
    }
}

/// One recorded track to process.
#[derive(Debug, Clone, PartialEq)]
pub struct PostTrack {
    /// The track row (its id goes into the segments).
    pub track: Track,
    /// The recording, frame 0 at meeting time zero.
    pub audio: MonoBuffer,
    /// Final-pass decoding options (from the [`FinalPassRequest`]).
    pub options: TranscribeOptions,
    /// What the live speech stretches decoded on this track, if they ran
    /// ([`crate::stretches`]); reused when it is the same transcription (module docs).
    pub stretches: Option<StoredStretches>,
    /// The speaker windows the live worker computed on this track, if it did
    /// ([`crate::speakers`]); reused when they are the final pass's (module docs).
    pub speakers: Option<StoredSpeakers>,
}

/// What [`PostProcessor::run`] processes.
#[derive(Debug, Clone, PartialEq)]
pub struct PostInput {
    /// The meeting.
    pub meeting: EntityId,
    /// The tracks to process, mic first (at most one per kind).
    pub tracks: Vec<PostTrack>,
    /// The echo canceller's delay lock at the end of the live session
    /// ([`crate::echo::echo_locks`]); empty: no echo path, nothing is cleaned as echo.
    pub echo_locks: Vec<EchoLock>,
}

/// One speaker of the processed tracks.
#[derive(Debug, Clone, PartialEq)]
pub struct PostSpeaker {
    /// The `Speaker` row (the `speaker_id` of its segments).
    pub id: EntityId,
    /// Role after naming.
    pub role: SpeakerRole,
    /// Track the speaker was found on.
    pub track: TrackId,
    /// The identified person, if any.
    pub person: Option<EntityId>,
    /// Name shown in the transcript.
    pub display_name: String,
    /// Total speech.
    pub speech: Duration,
}

/// Something worth telling the user; processing still succeeded.
#[derive(Debug, Clone, PartialEq)]
pub enum PostWarning {
    /// "Me" enrolment was on but the meeting had too little of the user's speech for a
    /// first voiceprint.
    NotEnoughSpeechForMe {
        /// Needed.
        needed: Duration,
        /// Available.
        got: Duration,
    },
    /// Stored voiceprints that could not be scored (corrupt rows) were skipped.
    SkippedVoiceprints(usize),
    /// The final-pass decoder reported a different backend after its calls than before
    /// (a per-call GPU fallback: whisper.cpp re-initialises its backend for beam search
    /// and may land on the CPU). The recorded pass backend is the one reported after.
    BackendChanged {
        /// Before the final pass.
        before: earshot_core::asr::BackendKind,
        /// After it.
        after: earshot_core::asr::BackendKind,
    },
    /// The final transcript was stored, but its device-local provenance (pass backend,
    /// echo report) could not be.
    ProvenanceNotStored(String),
    /// The echo speaker check could not run (the embedding model has no calibrated
    /// margin); only the text rule cleaned echo from the "Me" transcript.
    NoSpeakerEchoCheck {
        /// Why.
        reason: String,
    },
    /// A track's stored live speech stretches could not be read (another version,
    /// damaged); the track was transcribed whole. `reason` is exactly that track's
    /// [`StretchUse::Transcribed`] reason in [`PostOutcome::stretches`], so a host that
    /// shows the stretch use need not show this too.
    StretchRecordUnreadable {
        /// The track.
        track: TrackId,
        /// Why.
        reason: String,
    },
    /// A track's stored live speaker windows could not be read (another version, damaged);
    /// the track was diarised whole.
    SpeakerRecordUnreadable {
        /// The track.
        track: TrackId,
        /// Why.
        reason: String,
    },
    /// The speakers of an earlier final pass could not be carried over to this one (their
    /// names and Person links are not kept, module docs step 4): their identity record
    /// cannot be read, or they were found with another embedding model, whose centroids
    /// cannot be compared with this pass's.
    SpeakerNamesNotCarried {
        /// Why.
        reason: String,
    },
    /// Final-pass pieces whose every decode repeated itself (a whisper loop) were dropped:
    /// nothing is stored for these stretches ([`earshot_core::asr::transcribe_in_pieces`]).
    RepetitionDropped {
        /// The track.
        track: TrackId,
        /// The dropped stretches (meeting time).
        spans: Vec<(MeetingTime, MeetingTime)>,
    },
    /// Final segments whose text the user edited were kept as they are (module docs, step
    /// 5, T12.16), and the new transcript's words in their time left out.
    EditedLinesKept {
        /// Edited segments kept on the tracks this run processed.
        lines: usize,
        /// New words left out because they overlap an edited segment.
        words_dropped: usize,
    },
    /// Diarisation heard a voice that no transcribed word belongs to (nothing it said was
    /// transcribed, or every word went to another voice): it gets no speaker row.
    VoiceWithoutWords {
        /// The track it was heard on.
        track: TrackId,
        /// How much speech diarisation gave it.
        speech: Duration,
    },
}

/// Whether a track's final pass reused its live speech stretches (module docs, step 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StretchUse {
    /// The stretches' pieces were taken; only audio after `through` was transcribed.
    Reused {
        /// The stretches' decoded frontier (meeting time).
        through: MeetingTime,
        /// Pieces taken from the stretches (dropped ones included).
        pieces: usize,
        /// The backend that decoded them during the meeting.
        backend: BackendInfo,
    },
    /// The track was transcribed whole.
    Transcribed {
        /// Why the stretches were not reused (no record, another model, ...).
        reason: String,
    },
}

/// What the final pass still has to do on a track ([`PostProcessor::cleanup_work`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupWork {
    /// The track.
    pub track: TrackId,
    /// Audio left to transcribe after the reusable live speech stretches (all of it
    /// without them).
    pub to_transcribe: Duration,
    /// Audio left to diarise after the reusable live speaker windows (all of it without
    /// them).
    pub to_diarise: Duration,
}

/// Whether a track's diarisation reused its live speaker windows (module docs, step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeakerWindowUse {
    /// The first `windows` windows were taken from the live worker; the rest were computed
    /// here, then all were clustered.
    Reused {
        /// Windows taken.
        windows: usize,
        /// Why live speaker work stopped early, if it did.
        stopped: Option<String>,
    },
    /// The track was diarised whole.
    Diarised {
        /// Why the live windows were not reused (no record, another model, ...).
        reason: String,
    },
}

/// Result of a post-meeting run.
#[derive(Debug, Clone, PartialEq)]
pub struct PostOutcome {
    /// The meeting.
    pub meeting: EntityId,
    /// The speakers of the processed tracks (mic speakers first, then loopback).
    pub speakers: Vec<PostSpeaker>,
    /// Number of final segments written for the processed tracks.
    pub final_segments: usize,
    /// The user's voiceprint written or updated by "Me" enrolment.
    pub enrolled_me: Option<EntityId>,
    /// The backend that ran the final pass: the final-pass transcriber's, unless every
    /// processed track was taken whole from its live speech stretches (nothing decoded
    /// here) by one and the same live backend, then that backend.
    pub backend: BackendInfo,
    /// Non-fatal findings.
    pub warnings: Vec<PostWarning>,
    /// What echo cleaning removed from the final "Me" transcript (also stored,
    /// [`crate::echo::echo_report`]).
    pub echo: EchoReport,
    /// Per processed track (in input order), whether the live speech stretches were reused.
    pub stretches: Vec<(TrackId, StretchUse)>,
    /// Per processed track (in input order), whether the live speaker windows were reused.
    pub speaker_windows: Vec<(TrackId, SpeakerWindowUse)>,
}

/// A finished step of [`PostProcessor::run_reporting`], for progress output (T6.18).
///
/// Every step is reported exactly once, and `EchoChecked` then `Stored` come last.
/// Transcription and diarisation run at the same time (T6.12), so `Transcribed` and
/// `Diarised` steps interleave in any order. `Transcribed` steps come in track order.
/// `Diarised` steps come in track order for the tracks one diarizer handles; with a second
/// diarizer ([`PostProcessor::with_second_diarizer`]) the first track is diarised on one and
/// the others on the second at the same time, so the first track's step and the others' come
/// in either order.
#[derive(Debug, Clone, PartialEq)]
pub enum PostStage {
    /// A track was re-transcribed.
    Transcribed {
        /// The track.
        track: TrackId,
        /// Wall time of the step.
        elapsed: Duration,
    },
    /// A track's speakers were found.
    Diarised {
        /// The track.
        track: TrackId,
        /// Speakers found on it.
        speakers: usize,
        /// Wall time of the step.
        elapsed: Duration,
    },
    /// Echo was cleaned from the "Me" transcript.
    EchoChecked {
        /// Wall time of the step.
        elapsed: Duration,
    },
    /// Speakers were named and the final transcript stored.
    Stored {
        /// Wall time of the step.
        elapsed: Duration,
    },
}

/// Reads the recordings named by `request` back from the store: the track is the
/// request's `track_entity` when given (else the meeting's track of that kind), the audio
/// its `AudioFile` at the row's sample rate. Rows are read under the store lock; FLAC is
/// decoded after it is released.
///
/// Returns the input and the tracks of the request without a usable recording (no row,
/// or the file deleted by retention or by hand), which cannot be processed.
///
/// # Errors
/// Store errors; a recording that cannot be decoded or whose rate differs from its row.
pub fn load_post_input(
    services: &Services,
    meeting: EntityId,
    request: &FinalPassRequest,
) -> Result<(PostInput, Vec<TrackId>), RuntimeError> {
    let (tracks, files, echo_locks, records) = {
        let db = services.store();
        let mut records = Vec::new();
        for t in &request.tracks {
            records.push((
                t.track,
                stretch_record(&db, meeting, t.track)?,
                speaker_record(&db, meeting, t.track)?,
            ));
        }
        (
            db.tracks(meeting)?,
            db.audio_files(meeting)?,
            crate::echo::echo_locks(&db, meeting)?,
            records,
        )
    };
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for wanted in &request.tracks {
        let track = match wanted.track_entity {
            Some(id) => tracks.iter().find(|t| t.id == id),
            None => tracks.iter().find(|t| t.kind == wanted.track),
        };
        let Some((track, file)) =
            track.and_then(|t| files.iter().find(|f| f.track_id == t.id).map(|f| (t, f)))
        else {
            missing.push(wanted.track);
            continue;
        };
        let path = services.data.resolve(&file.path)?;
        if !path.exists() {
            missing.push(wanted.track);
            continue;
        }
        let (rate, samples) = read_flac(&path)?;
        if rate != file.sample_rate_hz {
            return Err(RuntimeError::Internal(
                "a recording's sample rate differs from its AudioFile row",
            ));
        }
        let rate = SampleRate::new(file.sample_rate_hz).ok_or(RuntimeError::Internal(
            "an AudioFile row has a zero sample rate",
        ))?;
        let record = records.iter().find(|(k, _, _)| *k == wanted.track);
        let stretches = record.and_then(|(_, r, _)| r.clone());
        let speakers = record.and_then(|(_, _, s)| s.clone());
        out.push(PostTrack {
            track: track.clone(),
            audio: MonoBuffer::new(rate, samples),
            options: wanted.options.clone(),
            stretches,
            speakers,
        });
    }
    out.sort_by_key(|t| t.track.kind);
    Ok((
        PostInput {
            meeting,
            tracks: out,
            echo_locks,
        },
        missing,
    ))
}

/// The post-meeting models and settings; see the module docs.
#[derive(Debug)]
pub struct PostProcessor<T, S, E> {
    transcriber: T,
    diarizer: Diarizer<S, E>,
    /// A second copy of the speaker models: the second track is diarised on it at the same
    /// time as the first ([`PostProcessor::with_second_diarizer`], T6.12).
    second: Option<Diarizer<S, E>>,
    voiceprint_threshold: f32,
    config: PostConfig,
    /// Decode each track in VAD speech pieces (T6.16); `None` decodes the whole track in one
    /// call. Production always sets it ([`crate::backends::post_processor`]).
    pieces: Option<Pieces>,
    /// ONNX Runtime threads of the speaker models; live speaker windows are reused only
    /// when they were computed with the same count ([`PostProcessor::with_speaker_threads`]).
    speaker_threads: Option<usize>,
}

/// The speech detector and settings for piece-wise final-pass decoding.
struct Pieces {
    vad: Box<dyn Vad + Send>,
    config: PieceConfig,
}

impl fmt::Debug for Pieces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pieces")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The live stretches of `t` that the final pass may take (module docs, step 1): the
/// record when it is the same transcription as the final pass of `t` with `model` and
/// `pieces`, else why not.
fn reusable<'a>(
    t: &'a PostTrack,
    model: &str,
    pieces: Option<&Pieces>,
    reuse_live: bool,
) -> Result<&'a StretchRecord, String> {
    if !reuse_live {
        return Err(WHOLE_PASS_REQUESTED.into());
    }
    let record = match t.stretches.as_ref().ok_or("no stretch record")? {
        StoredStretches::Record(r) => &**r,
        StoredStretches::Unreadable(why) => {
            return Err(format!("the stretch record cannot be read: {why}"));
        }
    };
    let pieces = pieces.ok_or("the final pass does not decode in pieces")?;
    if record.backend.model != model {
        return Err(format!(
            "the stretches ran model {}, the final pass runs {model}",
            record.backend.model
        ));
    }
    if record.options != t.options {
        return Err("the stretches used other decoding options".into());
    }
    if record.pieces != pieces.config {
        return Err("the stretches used other piece settings".into());
    }
    let vad = pieces.vad.id();
    if record.vad != vad {
        return Err(format!(
            "the stretches used VAD {}, the final pass uses {vad}",
            record.vad
        ));
    }
    if record.vad_frame != pieces.vad.frame_len() {
        return Err("the stretches used another VAD frame length".into());
    }
    record
        .decoded
        .check(t.audio.samples.len())
        .map_err(|e| format!("the stretch record does not fit the recording: {e}"))?;
    Ok(record)
}

/// What [`transcribe_tracks`] gives back: per track its transcription, warnings, and
/// whether its stretches were reused; and the stretches' backend when every track was
/// taken whole from them (nothing was decoded here).
type Transcribed = (
    Vec<Transcription>,
    Vec<PostWarning>,
    Vec<(TrackId, StretchUse)>,
    Option<BackendInfo>,
);

/// The final pass over every track, in order (step 1 of [`PostProcessor::run`]): each
/// track's transcription, plus a warning per track whose repetitive pieces were dropped
/// (by the final pass or by the reused stretches), and whether its stretches were reused.
fn transcribe_tracks<T: Transcriber>(
    transcriber: &mut T,
    pieces: &mut Option<Pieces>,
    tracks: &[PostTrack],
    reuse_live: bool,
    cancel: &CancelToken,
    progress: &std::sync::mpsc::Sender<PostStage>,
) -> Result<Transcribed, RuntimeError> {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let mut uses = Vec::new();
    // The live backend while no track needed a decode here.
    let mut all_reused: Option<Option<BackendInfo>> = None;
    for t in tracks {
        let started = Instant::now();
        let model = transcriber.backend_info().model;
        let reuse = reusable(t, &model, pieces.as_ref(), reuse_live);
        uses.push((
            t.track.kind,
            match &reuse {
                Ok(r) => StretchUse::Reused {
                    through: MeetingTime::from_pipeline_frames(
                        u64::try_from(r.decoded.through).unwrap_or(u64::MAX),
                    ),
                    pieces: r.decoded.pieces.len(),
                    backend: r.backend.clone(),
                },
                Err(reason) => StretchUse::Transcribed {
                    reason: reason.clone(),
                },
            },
        ));
        // An unreadable record is reported once: the warning carries the same reason as
        // the track's `StretchUse` (a host shows one of them, `PostWarning` docs).
        if let (Some(StoredStretches::Unreadable(_)), Err(reason)) = (&t.stretches, &reuse) {
            warnings.push(PostWarning::StretchRecordUnreadable {
                track: t.track.kind,
                reason: reason.clone(),
            });
        }
        let transcription = match pieces {
            Some(p) => {
                let stored = reuse.as_ref().ok().map(|r| r.decoded.pieces.len());
                let (transcription, report) = match reuse {
                    Ok(record) => transcribe_rest_in_pieces(
                        transcriber,
                        &mut p.vad,
                        &t.audio,
                        &t.options,
                        &p.config,
                        &record.decoded,
                        cancel,
                    ),
                    Err(_) => transcribe_in_pieces(
                        transcriber,
                        &mut p.vad,
                        &t.audio,
                        &t.options,
                        &p.config,
                        cancel,
                    ),
                }
                .map_err(RuntimeError::Asr)?;
                // The report counts the stored pieces too: none more means nothing was
                // decoded here.
                let whole = match (stored, &reuse) {
                    (Some(n), Ok(r)) if report.pieces == n => Some(r.backend.clone()),
                    _ => None,
                };
                all_reused = Some(match all_reused {
                    None => whole,
                    // Tracks taken whole from different live backends: no single live
                    // backend ran the pass, so it is reported as the final-pass
                    // transcriber's (each track's live backend is on its reuse line).
                    Some(first) => first.filter(|f| whole.as_ref() == Some(f)),
                });
                if !report.dropped.is_empty() {
                    warnings.push(PostWarning::RepetitionDropped {
                        track: t.track.kind,
                        spans: report.dropped,
                    });
                }
                transcription
            }
            None => {
                all_reused = Some(None);
                transcriber
                    .transcribe(&t.audio, &t.options, cancel)
                    .map_err(RuntimeError::Asr)?
            }
        };
        out.push(transcription);
        let _ = progress.send(PostStage::Transcribed {
            track: t.track.kind,
            elapsed: started.elapsed(),
        });
    }
    Ok((out, warnings, uses, all_reused.flatten()))
}

/// The live speaker windows of `t` that a diarizer with `params` and `threads` model
/// threads may finish from (module docs, step 2), else why not.
fn reusable_speakers<'a>(
    t: &'a PostTrack,
    params: &earshot_core::diar::WindowParams,
    threads: Option<usize>,
    reuse_live: bool,
) -> Result<&'a SpeakerRecord, String> {
    if !reuse_live {
        return Err(WHOLE_PASS_REQUESTED.into());
    }
    let record = match &t.speakers {
        None => return Err("no speaker record".into()),
        Some(StoredSpeakers::Unreadable(why)) => {
            return Err(format!("the speaker record cannot be read: {why}"));
        }
        // Speaker work was off during the meeting (the speaker detection setting): the
        // whole track is diarised here, for that reason.
        Some(StoredSpeakers::Off(why)) => return Err(why.clone()),
        Some(StoredSpeakers::Record(r)) => &**r,
    };
    let theirs = &record.header.params;
    if theirs.segmenter.model != params.segmenter.model {
        return Err(format!(
            "the live windows used segmentation model {}, the final pass uses {}",
            theirs.segmenter.model, params.segmenter.model
        ));
    }
    if theirs.embedder.model != params.embedder.model {
        return Err(format!(
            "the live windows used embedding model {}, the final pass uses {}",
            theirs.embedder.model, params.embedder.model
        ));
    }
    if theirs != params {
        return Err("the live windows used other window settings".into());
    }
    if record.header.threads != threads {
        return Err(format!(
            "the live speaker models ran with {:?} threads, the final pass's with {threads:?}",
            record.header.threads
        ));
    }
    if record.results.covered() > t.audio.samples.len() {
        return Err("the live windows cover more audio than the recording".into());
    }
    Ok(record)
}

/// Diarisation of every track, in order (step 2 of [`PostProcessor::run`]), each from its
/// live speaker windows when they can be reused, and whether they were.
fn diarise_tracks<S: SpeakerSegmenter, E: SpeakerEmbedder>(
    diarizer: &mut Diarizer<S, E>,
    threads: Option<usize>,
    tracks: &[PostTrack],
    reuse_live: bool,
    cancel: &CancelToken,
    progress: &std::sync::mpsc::Sender<PostStage>,
) -> Result<Vec<(TrackId, Diarization, SpeakerWindowUse)>, RuntimeError> {
    let mut out = Vec::new();
    for t in tracks {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Asr(AsrError::Cancelled));
        }
        let started = Instant::now();
        let reuse = reusable_speakers(t, &diarizer.window_params(), threads, reuse_live);
        let (d, used) = match reuse {
            Ok(record) => match diarizer.diarize_from(
                record.results.clone(),
                &t.audio,
                MeetingTime::ZERO,
                cancel,
            ) {
                Ok(d) => (
                    d,
                    SpeakerWindowUse::Reused {
                        windows: record.results.windows(),
                        stopped: record.header.stopped.clone(),
                    },
                ),
                // Results that do not fit after all: diarise whole, say why.
                Err(earshot_core::diar::DiarError::State(why)) => (
                    diarizer
                        .diarize(&t.audio, MeetingTime::ZERO, cancel)
                        .map_err(RuntimeError::Diar)?,
                    SpeakerWindowUse::Diarised { reason: why },
                ),
                Err(e) => return Err(RuntimeError::Diar(e)),
            },
            Err(reason) => (
                diarizer
                    .diarize(&t.audio, MeetingTime::ZERO, cancel)
                    .map_err(RuntimeError::Diar)?,
                SpeakerWindowUse::Diarised { reason },
            ),
        };
        let _ = progress.send(PostStage::Diarised {
            track: t.track.kind,
            speakers: d.speakers.len(),
            elapsed: started.elapsed(),
        });
        out.push((t.track.kind, d, used));
    }
    Ok(out)
}

/// Whether `e` is a cancellation (of either model).
pub(crate) fn is_cancellation(e: &RuntimeError) -> bool {
    matches!(
        e,
        RuntimeError::Asr(AsrError::Cancelled)
            | RuntimeError::Diar(earshot_core::diar::DiarError::Cancelled)
    )
}

/// What a [`PostJob`] gives back: the processor (for the next meeting) and the result.
pub type JobResult<T, S, E> = (PostProcessor<T, S, E>, Result<PostOutcome, RuntimeError>);

/// A post-meeting run on its own thread.
#[derive(Debug)]
pub struct PostJob<T, S, E> {
    cancel: CancelToken,
    thread: JoinHandle<JobResult<T, S, E>>,
}

impl<T, S, E> PostJob<T, S, E> {
    /// Cancels the running model call (the job then ends with a cancellation error and
    /// writes nothing).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Waits for the result; gives the processor back for the next meeting.
    ///
    /// # Errors
    /// [`RuntimeError::Poisoned`] if the job thread panicked.
    pub fn wait(self) -> Result<JobResult<T, S, E>, RuntimeError> {
        self.thread.join().map_err(|_| RuntimeError::Poisoned)
    }
}

/// How a diarised speaker is named.
struct Naming {
    role: SpeakerRole,
    person: Option<EntityId>,
    name: Option<String>,
}

impl<T, S, E> PostProcessor<T, S, E>
where
    T: Transcriber + Send,
    S: SpeakerSegmenter + Send,
    E: SpeakerEmbedder + Send,
{
    /// A processor over the final-pass transcriber and the diarisation models.
    ///
    /// # Errors
    /// [`RuntimeError::Uncalibrated`] if the embedder's model has no clustering or
    /// voiceprint threshold; [`RuntimeError::Diar`] for an invalid diarisation config.
    pub fn new(
        transcriber: T,
        segmenter: S,
        embedder: E,
        config: PostConfig,
    ) -> Result<Self, RuntimeError> {
        let model = embedder.info().model.clone();
        let clustering = config.clustering_threshold(&model)?;
        let voiceprint_threshold = config.voiceprint_threshold(&model)?;
        let mut diarization = config.diarization.clone();
        diarization.clustering.threshold = clustering;
        let diarizer =
            Diarizer::new(segmenter, embedder, diarization).map_err(RuntimeError::Diar)?;
        Ok(Self {
            transcriber,
            diarizer,
            second: None,
            voiceprint_threshold,
            config,
            pieces: None,
            speaker_threads: None,
        })
    }

    /// Records that the speaker models run with `threads` ONNX Runtime threads (production:
    /// [`crate::backends::speaker_threads`]); live speaker windows are reused only when they
    /// were computed with the same count (module docs, step 2).
    #[must_use]
    pub fn with_speaker_threads(mut self, threads: Option<usize>) -> Self {
        self.speaker_threads = threads;
        self
    }

    /// What live speaker windows must have been computed with to be reused here: the
    /// diarizer's [`Diarizer::window_params`] and the speaker models' thread count.
    #[must_use]
    pub fn speaker_window_key(&self) -> (earshot_core::diar::WindowParams, Option<usize>) {
        (self.diarizer.window_params(), self.speaker_threads)
    }

    /// What [`PostProcessor::run`] still has to do for each track of `input` once it takes
    /// the live work it can reuse (the same decisions as `run`, module docs steps 1 and 2):
    /// the audio it will transcribe and the audio it will diarise. For a cleanup-time
    /// estimate shown before the run (T6.18).
    #[must_use]
    pub fn cleanup_work(&self, input: &PostInput) -> Vec<CleanupWork> {
        let model = self.transcriber.backend_info().model;
        let params = self.diarizer.window_params();
        input
            .tracks
            .iter()
            .map(|t| {
                let len = t.audio.samples.len();
                let seconds = |n: usize| {
                    Duration::from_secs_f64(n as f64 / f64::from(t.audio.rate.hz().max(1)))
                };
                let transcribed = reusable(t, &model, self.pieces.as_ref(), self.config.reuse_live)
                    .map_or(0, |r| r.decoded.through.min(len));
                let diarised =
                    reusable_speakers(t, &params, self.speaker_threads, self.config.reuse_live)
                        .map_or(0, |r| r.results.covered().min(len));
                CleanupWork {
                    track: t.track.kind,
                    to_transcribe: seconds(len - transcribed),
                    to_diarise: seconds(len - diarised),
                }
            })
            .collect()
    }

    /// Diarises the second track on its own copy of the speaker models, at the same time as
    /// the first (T6.12). The copy must be the same embedding model; the result is the same
    /// as with one diarizer.
    ///
    /// # Errors
    /// [`RuntimeError::Internal`] for a different embedding model; [`RuntimeError::Diar`]
    /// for an invalid segmenter.
    pub fn with_second_diarizer(mut self, segmenter: S, embedder: E) -> Result<Self, RuntimeError> {
        if embedder.info().model != self.diarizer.embedder_mut().info().model {
            return Err(RuntimeError::Internal(
                "the second diarizer must use the same embedding model",
            ));
        }
        let config = self.diarizer.config().clone();
        self.second = Some(Diarizer::new(segmenter, embedder, config).map_err(RuntimeError::Diar)?);
        Ok(self)
    }

    /// Whether the second track is diarised on its own models
    /// ([`PostProcessor::with_second_diarizer`]).
    #[must_use]
    pub fn diarises_tracks_in_parallel(&self) -> bool {
        self.second.is_some()
    }

    /// Decodes each track in speech pieces found by `vad` instead of one whole-track call,
    /// so whisper cannot carry a repetition loop across a long recording (T6.16).
    ///
    /// # Errors
    /// [`RuntimeError::Asr`] for an invalid `config`.
    pub fn with_pieces(
        mut self,
        vad: Box<dyn Vad + Send>,
        config: PieceConfig,
    ) -> Result<Self, RuntimeError> {
        config.validate().map_err(RuntimeError::Asr)?;
        self.pieces = Some(Pieces { vad, config });
        Ok(self)
    }

    /// Whether tracks are decoded in speech pieces ([`PostProcessor::with_pieces`]).
    #[must_use]
    pub fn decodes_in_pieces(&self) -> bool {
        self.pieces.is_some()
    }

    /// Runs the post-meeting steps for `input` (module docs) and stores the result.
    ///
    /// # Errors
    /// [`RuntimeError::NoTracks`] for an input without tracks; model errors
    /// ([`RuntimeError::Asr`], [`RuntimeError::Diar`], cancellation as
    /// `Asr(Cancelled)`); voiceprint and store errors; [`RuntimeError::MeetingBusy`] while
    /// the meeting is being deleted, [`RuntimeError::Conflict`] if it was deleted by the
    /// time the result is stored. Nothing is written on error.
    pub fn run<R: Rng + ?Sized>(
        &mut self,
        services: &Services,
        input: PostInput,
        rng: &mut R,
        cancel: &CancelToken,
    ) -> Result<PostOutcome, RuntimeError> {
        self.run_reporting(services, input, rng, cancel, &mut |_| {})
    }

    /// [`PostProcessor::run`], calling `progress` after each step ([`PostStage`]).
    ///
    /// # Errors
    /// As [`PostProcessor::run`].
    pub fn run_reporting<R: Rng + ?Sized>(
        &mut self,
        services: &Services,
        input: PostInput,
        rng: &mut R,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(PostStage),
    ) -> Result<PostOutcome, RuntimeError> {
        if input.tracks.is_empty() {
            return Err(RuntimeError::NoTracks);
        }
        if cancel.is_cancelled() {
            return Err(RuntimeError::Asr(AsrError::Cancelled));
        }
        // The meeting cannot be deleted in this process while the pass runs (T6.26).
        let _lease = services.lease_meeting_for(input.meeting, crate::MeetingJob::Processing)?;
        let clock = std::sync::Arc::clone(&services.clock);

        // 1-3. The final pass and the per-track diarisation run at the same time (T6.12):
        // they read the same recordings and share no model, so the transcriber (the GPU on
        // the desktop) and the speaker models (the CPU) work side by side; the echo check and
        // labelling need both and come after. Progress is reported here as each step ends.
        // A failure in one stops the other, and its own error is returned. The backend is read
        // before and after: a decoder that fell back during a call (production: a per-call GPU
        // fallback) reports the backend that actually ran afterwards, and the change is a
        // warning, never silent.
        let backend_before = self.transcriber.backend_info().kind;
        let inner = CancelToken::new();
        let (tx, rx) = std::sync::mpsc::channel::<PostStage>();
        let (transcriber, pieces, diarizer, second) = (
            &mut self.transcriber,
            &mut self.pieces,
            &mut self.diarizer,
            self.second.as_mut(),
        );
        let threads = self.speaker_threads;
        let reuse_live = self.config.reuse_live;
        let tracks = &input.tracks;
        // With a second diarizer the first track goes to the first and the rest to the second.
        let split = if second.is_some() {
            1.min(tracks.len())
        } else {
            tracks.len()
        };
        let (first_tracks, rest) = tracks.split_at(split);
        let (asr, diar, diar_rest) = std::thread::scope(|scope| {
            let (asr_tx, asr_cancel) = (tx.clone(), inner.clone());
            let asr = scope.spawn(move || {
                let r = transcribe_tracks(
                    transcriber,
                    pieces,
                    tracks,
                    reuse_live,
                    &asr_cancel,
                    &asr_tx,
                );
                if r.is_err() {
                    asr_cancel.cancel();
                }
                r
            });
            let (diar_tx, diar_cancel) = (tx.clone(), inner.clone());
            let diar = scope.spawn(move || {
                let r = diarise_tracks(
                    diarizer,
                    threads,
                    first_tracks,
                    reuse_live,
                    &diar_cancel,
                    &diar_tx,
                );
                if r.is_err() {
                    diar_cancel.cancel();
                }
                r
            });
            let rest_cancel = inner.clone();
            let diar_rest = second.filter(|_| !rest.is_empty()).map(|d| {
                scope.spawn(move || {
                    let r = diarise_tracks(d, threads, rest, reuse_live, &rest_cancel, &tx);
                    if r.is_err() {
                        rest_cancel.cancel();
                    }
                    r
                })
            });
            loop {
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(stage) => progress(stage),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if cancel.is_cancelled() {
                            inner.cancel();
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            let diar = diar.join();
            let diar_rest = diar_rest.map(std::thread::ScopedJoinHandle::join);
            (asr.join(), diar, diar_rest)
        });
        let asr = asr.map_err(|_| RuntimeError::Poisoned)?;
        let diar = diar.map_err(|_| RuntimeError::Poisoned)?;
        let diar = match diar_rest.transpose().map_err(|_| RuntimeError::Poisoned)? {
            None => diar,
            Some(rest) => match (diar, rest) {
                (Ok(mut a), Ok(b)) => {
                    a.extend(b);
                    Ok(a)
                }
                (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(e),
                (Err(a), Err(b)) => Err(if is_cancellation(&a) { b } else { a }),
            },
        };
        if cancel.is_cancelled() {
            return Err(RuntimeError::Asr(AsrError::Cancelled));
        }
        let ((transcriptions, repetition_warnings, stretches, live_backend), diarised) =
            match (asr, diar) {
                (Ok(a), Ok(d)) => (a, d),
                // The step that failed first cancelled the other: report the real failure.
                (Err(e), Ok(_)) | (Ok(_), Err(e)) => return Err(e),
                (Err(a), Err(d)) => return Err(if is_cancellation(&a) { d } else { a }),
            };
        let mut diarizations = Vec::new();
        let mut speaker_windows = Vec::new();
        for (track, d, used) in diarised {
            diarizations.push((track, d));
            speaker_windows.push((track, used));
        }
        let unreadable_speakers: Vec<PostWarning> = input
            .tracks
            .iter()
            .filter_map(|t| match &t.speakers {
                Some(StoredSpeakers::Unreadable(reason)) => {
                    Some(PostWarning::SpeakerRecordUnreadable {
                        track: t.track.kind,
                        reason: reason.clone(),
                    })
                }
                _ => None,
            })
            .collect();
        // Segment ids in track order, as the sequential pass made them.
        let mut transcripts: Vec<(TrackId, Vec<Segment>)> = Vec::new();
        for (t, transcription) in input.tracks.iter().zip(transcriptions) {
            let mut next_id = || EntityId::generate(&*clock, &mut *rng);
            let segments = transcription.into_segments(t.track.id, MeetingTime::ZERO, &mut next_id);
            transcripts.push((t.track.kind, segments));
        }
        let after = self.transcriber.backend_info();
        let backend_change =
            (after.kind != backend_before).then_some(PostWarning::BackendChanged {
                before: backend_before,
                after: after.kind,
            });
        // Nothing decoded here: the pass backend is the live one that decoded every piece.
        let backend = live_backend.unwrap_or(after);

        // Echo cleaning of the mic ("Me") transcript, before labelling (T6.4g), with the
        // echo clusters dropped from the mic diarisation (T6.23a).
        let started = Instant::now();
        let MicEchoCleaning {
            report: echo,
            warning: echo_warning,
            me: echo_me,
        } = self.clean_echo(&input, &mut transcripts, &mut diarizations, cancel)?;
        progress(PostStage::EchoChecked {
            elapsed: started.elapsed(),
        });
        let store_started = Instant::now();

        // The policy follows the meeting's tracks, not only the ones processed now: a
        // re-run of one track of a two-track meeting keeps the two-track roles.
        let recorded: BTreeSet<TrackId> = services
            .store()
            .tracks(input.meeting)?
            .into_iter()
            .filter(|t| !t.meta.deleted)
            .map(|t| t.kind)
            .collect();
        let model = diarizations[0].1.embedding_model.clone();
        let none = Diarization {
            speakers: Vec::new(),
            turns: Vec::new(),
            embedding_model: model.clone(),
        };
        let two = recorded.contains(&TrackId::Mic) && recorded.contains(&TrackId::Reference);
        let find = |k: TrackId| diarizations.iter().find(|(t, _)| *t == k).map(|(_, d)| d);
        let mut meeting_diar = match (find(TrackId::Mic), find(TrackId::Reference)) {
            (Some(mic), Some(lb)) => two_track(mic, lb).map_err(RuntimeError::Diar)?,
            (Some(mic), None) if two => two_track(mic, &none).map_err(RuntimeError::Diar)?,
            (None, Some(lb)) if two => two_track(&none, lb).map_err(RuntimeError::Diar)?,
            (Some(one), None) => in_person(one, TrackId::Mic),
            (None, Some(one)) => in_person(one, TrackId::Reference),
            (None, None) => return Err(RuntimeError::NoTracks),
        };
        let processed: BTreeSet<EntityId> = input.tracks.iter().map(|t| t.track.id).collect();
        // Every recorded track is processed: all earlier speakers are this run's to replace.
        let full = recorded
            .iter()
            .all(|k| input.tracks.iter().any(|t| t.track.kind == *k));

        // 4-5. Naming and persistence in one transaction.
        let threshold = self.voiceprint_threshold;
        let config = &self.config;
        let meeting = input.meeting;
        let mut db = services.store();
        let (store, hlc) = db.parts();
        let outcome = store.transaction(|tx| -> Result<PostOutcome, RuntimeError> {
            // Deleted meanwhile (by another process: this one holds a lease): write nothing.
            if tx.meeting(meeting)?.is_none_or(|m| m.meta.deleted) {
                return Err(RuntimeError::Conflict("the meeting was deleted"));
            }
            let mut warnings: Vec<PostWarning> = backend_change
                .into_iter()
                .chain(echo_warning)
                .chain(repetition_warnings)
                .chain(unreadable_speakers)
                .collect();
            let mut stamp = Stamp {
                clock: &*clock,
                hlc,
                rng: &mut *rng,
            };

            // Earlier final rows: replaced on the processed tracks, kept elsewhere.
            let finals: Vec<SegmentRow> = tx
                .segments(meeting)?
                .into_iter()
                .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
                .collect();
            let (on_processed, kept): (Vec<SegmentRow>, Vec<SegmentRow>) = finals
                .into_iter()
                .partition(|r| processed.contains(&r.segment.track));
            // The user's edited text stays (module docs, step 5, T12.16).
            let (edited, replaced): (Vec<SegmentRow>, Vec<SegmentRow>) = on_processed
                .into_iter()
                .partition(|r| r.text_edited_at.is_some());
            let kept_speakers: BTreeSet<EntityId> =
                kept.iter().filter_map(|r| r.segment.speaker_id).collect();
            // Speakers of edited segments may still be continued by a new voice, so they
            // count as replaced, but their rows are never tombstoned.
            let edited_speakers: BTreeSet<EntityId> =
                edited.iter().filter_map(|r| r.segment.speaker_id).collect();
            let replaced_speakers: BTreeSet<EntityId> = replaced
                .iter()
                .filter_map(|r| r.segment.speaker_id)
                .chain(edited_speakers.iter().copied())
                .collect();
            let earlier: Vec<Speaker> = tx
                .speakers(meeting)?
                .into_iter()
                .filter(|s| !s.meta.deleted)
                .collect();
            let earlier_me = earlier
                .iter()
                .find(|s| s.role == SpeakerRole::Me)
                .map(|s| (s.id, s.person_id, s.display_name.clone()));
            let mut next_unknown = earlier
                .iter()
                .filter(|s| kept_speakers.contains(&s.id) || edited_speakers.contains(&s.id))
                .filter_map(|s| speaker_number(&s.display_name))
                .max()
                .unwrap_or(0);

            let mut namings =
                self::name_speakers(&*tx, &mut meeting_diar, &model, threshold, &mut warnings)?;
            // Neither the policy nor a voiceprint named "Me": the speaker check may, when it
            // discriminated (one mic voice passes its user test). Never enrolled from.
            let check_named_me = meeting_diar.me().is_none()
                && echo_me
                    .as_deref()
                    .is_some_and(|me| promote_checked_me(&mut meeting_diar, &mut namings, me));
            // The transcript labelled with its diarisation speakers before any speaker row is
            // written (T7.3b): a voice that owns no segment (heard, but no word of it was
            // transcribed) gets no row, takes no part in carrying earlier names, and is
            // reported instead.
            let edited_segments: Vec<Segment> = edited.iter().map(|r| r.segment.clone()).collect();
            let mut words_under_edits = 0;
            let mut labelled: Vec<(Segment, Option<usize>)> = Vec::new();
            for (kind, segments) in transcripts {
                let (segments, dropped) = drop_words_under_edits(segments, &edited_segments);
                words_under_edits += dropped;
                let turns: Vec<SpeakerTurn> = meeting_diar
                    .turns
                    .iter()
                    .filter(|t| t.track == kind)
                    .map(|t| SpeakerTurn {
                        speaker: t.speaker,
                        start: t.start,
                        end: t.end,
                    })
                    .collect();
                let mut next_id = || EntityId::generate(stamp.clock, &mut *stamp.rng);
                labelled.extend(label_segment_speakers(segments, &turns, &mut next_id));
            }
            if !edited.is_empty() {
                warnings.push(PostWarning::EditedLinesKept {
                    lines: edited.len(),
                    words_dropped: words_under_edits,
                });
            }
            let worded: BTreeSet<usize> = labelled.iter().filter_map(|(_, k)| *k).collect();
            for (k, s) in meeting_diar.speakers.iter().enumerate() {
                if !worded.contains(&k) {
                    warnings.push(PostWarning::VoiceWithoutWords {
                        track: s.track,
                        speech: s.speech,
                    });
                }
            }
            // One "Me" row per meeting: a new "Me" reuses the earlier one (and its name).
            let reusable_me =
                earlier_me.filter(|_| meeting_diar.me().is_some_and(|k| worded.contains(&k)));
            // Earlier speakers this run replaces. On a partial re-run that is only the
            // speakers of the replaced segments (a speaker row has no track, so one without
            // segments may belong to an unprocessed track).
            let ours = |s: &Speaker| {
                (full || replaced_speakers.contains(&s.id)) && !kept_speakers.contains(&s.id)
            };

            // Speaker identities of the earlier runs (T6.25, step 4): a new cluster that
            // sounds like an earlier speaker this run replaces continues its row, keeping a
            // name or Person link the user gave it.
            let key = crate::identities::record_key(meeting);
            let previous: Vec<SpeakerIdentity> = match tx
                .local_value(key.as_str())?
                .map(|b| crate::identities::decode(&b))
            {
                None => Vec::new(),
                Some(StoredIdentities::Record(r)) => r.speakers,
                Some(StoredIdentities::Unreadable(reason)) => {
                    warnings.push(PostWarning::SpeakerNamesNotCarried { reason });
                    Vec::new()
                }
            };
            let mut candidates: Vec<(&Speaker, &SpeakerIdentity)> = Vec::new();
            let mut other_models: BTreeSet<String> = BTreeSet::new();
            for identity in &previous {
                let Some(row) = earlier.iter().find(|s| s.id == identity.speaker) else {
                    continue;
                };
                // "Me" always stays the Me row and is not matched, with one exception: a voice
                // the user marked as themselves in the room (T9.12), the only kind of Me row
                // with an entry, which continues like any edit while this run found no "Me" of
                // its own.
                let marked =
                    in_room_kinds(&recorded) && identity.user_edited && meeting_diar.me().is_none();
                if (row.role == SpeakerRole::Me && !marked) || !ours(row) {
                    continue;
                }
                // Centroids of another model cannot be compared: no carry-over.
                if identity.centroid.model != model {
                    other_models.insert(identity.centroid.model.to_string());
                    continue;
                }
                candidates.push((row, identity));
            }
            if !other_models.is_empty() {
                let theirs: Vec<String> = other_models.into_iter().collect();
                warnings.push(PostWarning::SpeakerNamesNotCarried {
                    reason: format!(
                        "the earlier speakers were found with embedding model {}, this pass \
                         uses {model}",
                        theirs.join(", ")
                    ),
                });
            }
            let new: Vec<Option<(TrackId, &earshot_core::diar::Embedding)>> = meeting_diar
                .speakers
                .iter()
                .zip(&namings)
                .enumerate()
                .map(|(k, (s, n))| {
                    (n.role != SpeakerRole::Me && worded.contains(&k))
                        .then_some((s.track, &s.centroid))
                })
                .collect();
            let matchable: Vec<Candidate<'_>> = candidates
                .iter()
                .map(|(row, identity)| Candidate {
                    track: identity.track,
                    centroid: &identity.centroid,
                    edited: identity.edited(row),
                })
                .collect();
            let continued =
                match_previous(&new, &matchable, threshold).map_err(RuntimeError::Diar)?;
            let continued: Vec<Option<(&Speaker, &SpeakerIdentity)>> = continued
                .into_iter()
                .map(|m| m.map(|j| candidates[j]))
                .collect();
            let continued_ids: BTreeSet<EntityId> =
                continued.iter().flatten().map(|(row, _)| row.id).collect();
            // A carried "Speaker N" the user chose is never given to a new speaker.
            for (row, identity) in continued.iter().flatten() {
                if identity.edited(row)
                    && let Some(n) = speaker_number(&row.display_name)
                {
                    next_unknown = next_unknown.max(n);
                }
            }

            // Speaker rows: earlier ones this run replaces and does not continue become
            // tombstones.
            let mut tombstoned = BTreeSet::new();
            for old in &earlier {
                let reused = reusable_me.as_ref().is_some_and(|(id, _, _)| *id == old.id);
                if ours(old)
                    && !reused
                    && !continued_ids.contains(&old.id)
                    && !edited_speakers.contains(&old.id)
                {
                    let mut old = old.clone();
                    old.meta.mark_deleted(stamp.hlc, stamp.clock);
                    tombstoned.insert(old.id);
                    tx.put_speaker(old)?;
                }
            }
            let mut speakers = Vec::new();
            // The row id of each diarisation speaker (`None`: no words, no row).
            let mut row_ids: Vec<Option<EntityId>> = vec![None; meeting_diar.speakers.len()];
            let mut identities = Vec::new();
            for (k, ((s, naming), earlier_speaker)) in meeting_diar
                .speakers
                .iter()
                .zip(namings)
                .zip(continued)
                .enumerate()
            {
                if !worded.contains(&k) {
                    continue;
                }
                // The user's name and Person link are kept as they are; only the role
                // follows this run's policy. A voice the user marked as themselves stays the
                // Me row: its diarisation speaker stays a room voice, so nothing is enrolled.
                if let Some((row, identity)) = earlier_speaker
                    && identity.edited(row)
                {
                    let mut row = row.clone();
                    let role = if row.role == SpeakerRole::Me {
                        SpeakerRole::Me
                    } else {
                        naming.role
                    };
                    if row.role != role {
                        row.role = role;
                        row.meta.touch(stamp.hlc, stamp.clock);
                        tx.put_speaker(row.clone())?;
                    }
                    if s.centroid.is_valid() {
                        identities.push(SpeakerIdentity {
                            speaker: row.id,
                            track: s.track,
                            centroid: s.centroid.clone(),
                            given_name: identity.given_name.clone(),
                            given_person: identity.given_person,
                            // The user's edit, kept: an edit for the next run too.
                            given_stamp: None,
                            user_edited: true,
                            // A new centroid: "remember these voices" may add it.
                            enrolled: false,
                        });
                    }
                    row_ids[k] = Some(row.id);
                    speakers.push(PostSpeaker {
                        id: row.id,
                        role,
                        track: s.track,
                        person: row.person_id,
                        display_name: row.display_name,
                        speech: s.speech,
                    });
                    continue;
                }
                if naming.role == SpeakerRole::Me
                    && let Some((id, person, name)) = &reusable_me
                {
                    row_ids[k] = Some(*id);
                    speakers.push(PostSpeaker {
                        id: *id,
                        role: SpeakerRole::Me,
                        track: s.track,
                        person: *person,
                        display_name: name.clone(),
                        speech: s.speech,
                    });
                    continue;
                }
                let (person, display_name) = if naming.role == SpeakerRole::Me {
                    let me = me_person(tx, &mut stamp).map_err(RuntimeError::Voiceprint)?;
                    (Some(me.id), me.display_name)
                } else if let (Some(p), Some(name)) = (naming.person, naming.name) {
                    (Some(p), name)
                } else {
                    next_unknown += 1;
                    (None, format!("Speaker {next_unknown}"))
                };
                // An earlier speaker without an edit: its row, named by this run.
                let row = match earlier_speaker {
                    Some((row, _)) => {
                        let mut row = row.clone();
                        if (row.role, &row.display_name, row.person_id)
                            != (naming.role, &display_name, person)
                        {
                            row.role = naming.role;
                            row.display_name.clone_from(&display_name);
                            row.person_id = person;
                            row.meta.touch(stamp.hlc, stamp.clock);
                            tx.put_speaker(row.clone())?;
                        }
                        row
                    }
                    None => {
                        let row = Speaker::new(
                            meeting,
                            naming.role,
                            display_name.clone(),
                            person,
                            &mut stamp,
                        );
                        tx.put_speaker(row.clone())?;
                        row
                    }
                };
                if naming.role != SpeakerRole::Me && s.centroid.is_valid() {
                    identities.push(SpeakerIdentity {
                        speaker: row.id,
                        track: s.track,
                        centroid: s.centroid.clone(),
                        given_name: display_name.clone(),
                        given_person: person,
                        // The row as this pass left it: any later write is an edit.
                        given_stamp: Some(row.meta.updated_at),
                        user_edited: false,
                        enrolled: false,
                    });
                }
                row_ids[k] = Some(row.id);
                speakers.push(PostSpeaker {
                    id: row.id,
                    role: naming.role,
                    track: s.track,
                    person,
                    display_name,
                    speech: s.speech,
                });
            }

            // The identities for the next run: this run's speakers, and the earlier entries
            // of speakers it left alone (another track's, on a partial re-run). Entries of
            // tombstoned speakers are dropped (their centroids are biometric data).
            let ours_now: BTreeSet<EntityId> = speakers.iter().map(|s| s.id).collect();
            let mut record: Vec<SpeakerIdentity> = previous
                .into_iter()
                .filter(|i| !ours_now.contains(&i.speaker) && !tombstoned.contains(&i.speaker))
                .filter(|i| earlier.iter().any(|s| s.id == i.speaker))
                .collect();
            record.extend(identities);
            if record.is_empty() {
                tx.delete_local_value(key.as_str())?;
            } else {
                crate::local::set_tx(
                    tx,
                    &key,
                    &crate::identities::encode(&IdentityRecord { speakers: record }),
                )?;
            }

            // Final segments of the processed tracks.
            for mut old in replaced {
                old.meta.mark_deleted(stamp.hlc, stamp.clock);
                tx.put_segment(old)?;
            }
            let mut labelled: Vec<Segment> = labelled
                .into_iter()
                .map(|(mut segment, k)| {
                    segment.speaker_id = k.and_then(|k| row_ids[k]);
                    segment
                })
                .collect();
            labelled.sort_by_key(|s| (s.start, s.id));
            let final_segments = labelled.len();
            for segment in labelled {
                let row = SegmentRow::new(
                    meeting,
                    TranscriptPass::Final,
                    segment,
                    stamp.hlc,
                    stamp.clock,
                );
                tx.put_segment(row)?;
            }

            let mut enrolled_me = None;
            if config.enroll_me
                && !check_named_me
                && let Some(k) = meeting_diar.me()
            {
                let me = &meeting_diar.speakers[k];
                // The reused "Me" row's person (the user may have re-linked it), else the
                // user's person.
                let result = match reusable_me.as_ref().and_then(|(_, person, _)| *person) {
                    Some(person) => enroll(
                        tx,
                        person,
                        std::slice::from_ref(&me.centroid),
                        me.speech,
                        &config.enrolment,
                        &mut stamp,
                    ),
                    None => enroll_me(
                        tx,
                        std::slice::from_ref(&me.centroid),
                        me.speech,
                        &config.enrolment,
                        &mut stamp,
                    )
                    .map(|(_, voiceprint)| voiceprint),
                };
                match result {
                    Ok(voiceprint) => enrolled_me = Some(voiceprint),
                    Err(VoiceprintError::NotEnoughSpeech { needed, got }) => {
                        warnings.push(PostWarning::NotEnoughSpeechForMe { needed, got });
                    }
                    Err(e) => return Err(RuntimeError::Voiceprint(e)),
                }
            }
            // The live records are transient (speaker embeddings are biometric data, the
            // stretches hold the raw transcript with echo words): they go with the final
            // pass, on every path (reused, fallback, unreadable), in the same transaction.
            // A failed or cancelled final pass writes nothing, so they stay for a retry.
            for t in &input.tracks {
                tx.delete_local_value(crate::speakers::record_key(meeting, t.track.kind).as_str())?;
                tx.delete_local_value(
                    crate::stretches::record_key(meeting, t.track.kind).as_str(),
                )?;
            }
            // The final transcript is stored: the audio retention time starts here, whatever
            // the summary does later (T7.4k); a re-run leaves an earlier
            // clock as it is.
            crate::retention::start_clock_tx(tx, meeting, stamp.clock.now_unix_ms())?;
            Ok(PostOutcome {
                meeting,
                speakers,
                final_segments,
                enrolled_me,
                backend,
                warnings,
                echo,
                stretches,
                speaker_windows,
            })
        })?;
        // The deleted live records leave the WAL too (as `forget_voiceprints` does). A reader
        // blocking the checkpoint makes it fail retryably; the store's closing checkpoint
        // then removes them.
        let _ = db.flush_erasures();
        // Device-local provenance (the echo report, which backend ran the final pass). The
        // transcript is already committed; a failure here is reported, not fatal.
        let mut outcome = outcome;
        let stored = record_echo_report(&mut db, meeting, TranscriptPass::Final, &outcome.echo)
            .and_then(|()| {
                crate::backends::record_pass_backend(
                    &mut db,
                    meeting,
                    TranscriptPass::Final,
                    None,
                    &outcome.backend,
                )
            });
        if let Err(e) = stored {
            outcome
                .warnings
                .push(PostWarning::ProvenanceNotStored(e.to_string()));
        }
        // The store is free before the last step is reported: a host's progress handler may
        // read it (T7.2a, `Services::meeting_activity`).
        drop(db);
        progress(PostStage::Stored {
            elapsed: store_started.elapsed(),
        });
        Ok(outcome)
    }
}

impl<T, S, E> PostProcessor<T, S, E>
where
    T: Transcriber,
    S: SpeakerSegmenter,
    E: SpeakerEmbedder,
{
    /// Removes far-end echo from the mic transcript and drops the echo clusters from the
    /// mic diarisation: [`clean_mic_echo`] over this pass's tracks (the first mic and
    /// loopback transcript and diarisation, the mic recording, this processor's embedder
    /// and voiceprint threshold).
    fn clean_echo(
        &mut self,
        input: &PostInput,
        transcripts: &mut [(TrackId, Vec<Segment>)],
        diarizations: &mut [(TrackId, Diarization)],
        cancel: &CancelToken,
    ) -> Result<MicEchoCleaning, RuntimeError> {
        // `MicEcho` holds one mic and one reference: a second track of either kind would be
        // left uncleaned without a word.
        let count = |kind| input.tracks.iter().filter(|t| t.track.kind == kind).count();
        debug_assert!(
            count(TrackId::Mic) <= 1,
            "echo cleaning handles at most one mic track, got {}",
            count(TrackId::Mic)
        );
        debug_assert!(
            count(TrackId::Reference) <= 1,
            "echo cleaning handles at most one reference track, got {}",
            count(TrackId::Reference)
        );
        let mic = transcripts.iter().position(|(k, _)| *k == TrackId::Mic);
        let reference: Option<Vec<Word>> = transcripts
            .iter()
            .find(|(k, _)| *k == TrackId::Reference)
            .map(|(_, s)| s.iter().flat_map(|s| s.words.iter().cloned()).collect());
        // The first mic and reference diarisations, borrowed apart (no copy).
        let (mut mic_diar, mut ref_diar) = (None, None);
        for (k, d) in diarizations.iter_mut() {
            if *k == TrackId::Mic && mic_diar.is_none() {
                mic_diar = Some(d);
            } else if *k == TrackId::Reference && ref_diar.is_none() {
                ref_diar = Some(&*d);
            }
        }
        let mic_audio = input
            .tracks
            .iter()
            .find(|t| t.track.kind == TrackId::Mic)
            .map(|t| &t.audio);
        let mut segments = mic.map(|i| std::mem::take(&mut transcripts[i].1));
        let out = clean_mic_echo(
            MicEcho {
                locks: &input.echo_locks,
                mic: segments.as_mut(),
                reference: reference.as_deref(),
                mic_diar,
                ref_diar,
                mic_audio,
                voiceprint_threshold: self.voiceprint_threshold,
            },
            self.diarizer.embedder_mut(),
            cancel,
        );
        if let (Some(i), Some(s)) = (mic, segments) {
            transcripts[i].1 = s;
        }
        out
    }
}

/// The tracks of one meeting that [`clean_mic_echo`] cleans.
pub struct MicEcho<'a> {
    /// The lock stretches of the live session ([`PostInput::echo_locks`]).
    pub locks: &'a [EchoLock],
    /// The mic transcript, cleaned in place; `None` when the mic is not processed.
    pub mic: Option<&'a mut Vec<Segment>>,
    /// The loopback transcript's words; `None` when the loopback is not processed.
    pub reference: Option<&'a [Word]>,
    /// The mic diarisation; its echo clusters are dropped in place.
    pub mic_diar: Option<&'a mut Diarization>,
    /// The loopback diarisation.
    pub ref_diar: Option<&'a Diarization>,
    /// The mic recording the speaker check embeds, frame 0 at meeting time zero (what the
    /// diarisation saw: the stored FLAC).
    pub mic_audio: Option<&'a MonoBuffer>,
    /// The embedding model's open-set voiceprint threshold (`drop_echo_clusters` rule 2).
    pub voiceprint_threshold: f32,
}

/// What [`clean_mic_echo`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct MicEchoCleaning {
    /// What was removed, by which rule, and why the speaker check did not run.
    pub report: EchoReport,
    /// [`PostWarning::NoSpeakerEchoCheck`] for an embedding model without a calibrated
    /// speaker-check margin.
    pub warning: Option<PostWarning>,
    /// Mic speakers (indices into the mic diarisation after its echo clusters were
    /// dropped) the speaker check found to be the user ([`me_speakers`]); `None` when the
    /// check did not run.
    pub me: Option<Vec<usize>>,
}

impl MicEchoCleaning {
    fn text_only(report: EchoReport, warning: Option<PostWarning>) -> Self {
        Self {
            report,
            warning,
            me: None,
        }
    }
}

/// The final pass's echo cleaning of the mic ("Me") transcript (T6.4g, T6.23, T6.23a),
/// public so that evaluation tools run exactly this: the text rule ([`text_echo_mask`]:
/// [`earshot_core::asr::dedupe_echo`] per stretch) and then the speaker check
/// ([`speaker_echo_spans`] on [`echo_span_pieces`] of the mic turns), each per cleaning
/// stretch ([`cleaning_stretches`] of the recorded locks: short gaps at the same delay
/// bridged) with that stretch's delay: mic words (and spans) outside every stretch are
/// never removed. Both need a mic and a loopback transcript. An embedding model without a
/// calibrated margin gets no speaker check, reported as [`PostWarning::NoSpeakerEchoCheck`]
/// (the text rule still runs). When anything was removed, the echo clusters are then
/// dropped from the mic diarisation (`drop_echo_clusters`) and the words a mostly-echo
/// cluster kept are removed ([`EchoReport::cluster_removed`]).
///
/// The returned `me` (mic voices that pass the check's user test) is on purpose the union
/// of [`me_speakers`] over all stretches, each judged over the whole meeting with that
/// stretch's delay, not a per-stretch answer: a voice that is the user under any lock is
/// not dropped for having lost every word (only a mostly-echo cluster that sounds like the
/// far end is, `drop_echo_clusters` rule 2), and more than one voice in `me` keeps the
/// check from naming "Me" (`promote_checked_me`).
///
/// # Errors
/// [`RuntimeError::Asr`] from the text rule, [`RuntimeError::Diar`] from the embedder, the
/// speaker check or a model mismatch between the two diarisations.
pub fn clean_mic_echo<E: SpeakerEmbedder + ?Sized>(
    input: MicEcho<'_>,
    embedder: &mut E,
    cancel: &CancelToken,
) -> Result<MicEchoCleaning, RuntimeError> {
    let MicEcho {
        locks,
        mic,
        reference,
        mut mic_diar,
        ref_diar,
        mic_audio,
        voiceprint_threshold,
    } = input;
    let mut report = EchoReport {
        locks: locks.to_vec(),
        bridged_gaps: bridged_gaps(locks),
        ..EchoReport::default()
    };
    if locks.is_empty() {
        report.speaker_check_skipped = Some("no echo-path lock".into());
        return Ok(MicEchoCleaning::text_only(report, None));
    }
    let (Some(mic), Some(reference)) = (mic, reference) else {
        report.speaker_check_skipped =
            Some("the mic and the loopback are not processed together".into());
        return Ok(MicEchoCleaning::text_only(report, None));
    };
    let before = mic.clone();
    let MicEchoCleaning {
        mut report,
        warning,
        me,
    } = text_and_speaker_rules(
        report,
        locks,
        mic,
        reference,
        mic_diar.as_deref(),
        ref_diar,
        mic_audio,
        embedder,
        cancel,
    )?;
    // A mic cluster whose words were all echo is not a speaker: drop it, so a single
    // remaining mic voice is "Me" under two_track (T6.4g). The words a mostly-echo
    // cluster kept are far-end words: removed and counted (T6.23a).
    let me = match mic_diar.take() {
        Some(d) => {
            let dropped = drop_echo_clusters(
                d,
                ref_diar,
                voiceprint_threshold,
                &cleaning_stretches(locks),
                &before,
                mic,
                me.as_deref(),
                mic_audio,
            )
            .map_err(RuntimeError::Diar)?;
            let (kept, removed) = drop_masked(mic, &dropped.cluster_words);
            *mic = kept;
            report.cluster_removed = removed;
            dropped.me
        }
        None => me,
    };
    Ok(MicEchoCleaning {
        report,
        warning,
        me,
    })
}

/// The text rule and the speaker check of [`clean_mic_echo`] on `mic`.
#[allow(clippy::too_many_arguments)]
fn text_and_speaker_rules<E: SpeakerEmbedder + ?Sized>(
    mut report: EchoReport,
    locks: &[EchoLock],
    mic: &mut Vec<Segment>,
    reference: &[Word],
    mic_diar: Option<&Diarization>,
    ref_diar: Option<&Diarization>,
    audio: Option<&MonoBuffer>,
    embedder: &mut E,
    cancel: &CancelToken,
) -> Result<MicEchoCleaning, RuntimeError> {
    // Text rule, per lock stretch.
    let words: Vec<Word> = mic.iter().flat_map(|s| s.words.iter().cloned()).collect();
    let echo_words = text_echo_mask(&words, reference, locks).map_err(RuntimeError::Asr)?;
    let (cleaned, removed) = drop_masked(mic, &echo_words);
    *mic = cleaned;
    report.text_removed = removed;

    let (Some(mic_diar), Some(ref_diar), Some(audio)) = (mic_diar, ref_diar, audio) else {
        report.speaker_check_skipped = Some("no diarisation of both tracks".into());
        return Ok(MicEchoCleaning::text_only(report, None));
    };
    let config = match SpeakerEchoConfig::for_model(&mic_diar.embedding_model) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("{e:?}");
            report.speaker_check_skipped = Some(reason.clone());
            let warning = PostWarning::NoSpeakerEchoCheck { reason };
            return Ok(MicEchoCleaning::text_only(report, Some(warning)));
        }
    };
    let samples = &audio.samples;
    let pieces = echo_span_pieces(
        &mic_diar.turns,
        audio.rate,
        samples.len(),
        embedder.info().min_samples,
    );
    let slices: Vec<&[f32]> = pieces.iter().map(|p| &samples[p.samples.clone()]).collect();
    let embeddings = if slices.is_empty() {
        Vec::new()
    } else {
        embedder
            .embed(&slices, cancel)
            .map_err(RuntimeError::Diar)?
    };
    let spans: Vec<SpanEmbedding> = pieces
        .into_iter()
        .zip(embeddings)
        .filter(|(_, e)| e.is_valid())
        .map(|(p, embedding)| SpanEmbedding {
            start: p.start,
            end: p.end,
            embedding,
        })
        .collect();
    // Speaker check, per lock stretch: a span is judged with the delay of the stretch
    // its midpoint lies in; a span outside every stretch stays clean.
    let mut echo = vec![false; spans.len()];
    let mut me: Vec<usize> = Vec::new();
    for lock in &cleaning_stretches(locks) {
        let inside: Vec<usize> = (0..spans.len())
            .filter(|&i| {
                let mid = (spans[i].start.as_millis() + spans[i].end.as_millis()) / 2;
                lock.covers(mid)
            })
            .collect();
        if inside.is_empty() {
            continue;
        }
        let cfg = SpeakerEchoConfig {
            delay: Some(lock.delay()),
            ..config
        };
        let subset: Vec<SpanEmbedding> = inside.iter().map(|&i| spans[i].clone()).collect();
        let flags =
            speaker_echo_spans(mic_diar, ref_diar, &subset, &cfg).map_err(RuntimeError::Diar)?;
        for (&i, f) in inside.iter().zip(flags) {
            echo[i] |= f;
        }
        for k in me_speakers(mic_diar, ref_diar, &cfg).map_err(RuntimeError::Diar)? {
            if !me.contains(&k) {
                me.push(k);
            }
        }
    }
    let mut kept_segments = Vec::new();
    for s in std::mem::take(mic) {
        let out = remove_speaker_echo(&s.words, &spans, &echo);
        report.speaker_removed += out.removed.len();
        if out.removed.is_empty() {
            kept_segments.push(s);
        } else if !out.kept.is_empty() {
            let mut seg = Segment::from_words(s.id, s.track, out.kept, s.start);
            seg.speaker_id = s.speaker_id;
            kept_segments.push(seg);
        }
    }
    *mic = kept_segments;
    Ok(MicEchoCleaning {
        report,
        warning: None,
        me: Some(me),
    })
}

/// For each of `words`, whether it is missing from `kept` (an order-preserving
/// subsequence of `words`).
/// `segments` without the words `mask` marks (in order over all their words); a segment
/// left without words is dropped. Returns the segments and the number of words removed.
fn drop_masked(segments: &[Segment], mask: &[bool]) -> (Vec<Segment>, usize) {
    let mut out = Vec::new();
    let (mut k, mut removed) = (0, 0);
    for s in segments {
        let mut kept = Vec::new();
        for w in &s.words {
            if mask.get(k).copied().unwrap_or(false) {
                removed += 1;
            } else {
                kept.push(w.clone());
            }
            k += 1;
        }
        if kept.len() == s.words.len() {
            out.push(s.clone());
        } else if !kept.is_empty() {
            let mut seg = Segment::from_words(s.id, s.track, kept, s.start);
            seg.speaker_id = s.speaker_id;
            out.push(seg);
        }
    }
    (out, removed)
}

/// Whether `w` lies under the span of the edited segment `e` (same track checked by the
/// caller): their times overlap by more than nothing, or, for an untimed word (`start ==
/// end`), it starts inside `[e.start, e.end)`. An edited segment with no length covers
/// nothing.
fn under_edit(w: &Word, e: &Segment) -> bool {
    if e.end <= e.start {
        return false;
    }
    if w.end <= w.start {
        return e.start <= w.start && w.start < e.end;
    }
    w.start < e.end && w.end > e.start
}

/// A new decode giving way to the user's edited segments (T12.16, module docs step 5):
/// every word of `segments` under the span of an `edited` segment on the same track
/// ([`under_edit`]) is left out; a segment that loses some words is rebuilt from the rest
/// (same id, track and speaker, span and text from the words that stay), one that loses all
/// is dropped. A new segment with no words is judged by its own span, as one word. The rule
/// is per word, not per segment, because a re-run cuts segments differently: a new segment
/// that runs past the edited one keeps the words outside it. A word straddling the edited
/// span's edge goes, because the edited text already covers that stretch of audio. Returns
/// the segments and how many words were left out.
pub(crate) fn drop_words_under_edits(
    segments: Vec<Segment>,
    edited: &[Segment],
) -> (Vec<Segment>, usize) {
    if edited.is_empty() {
        return (segments, 0);
    }
    let mut kept_segments = Vec::with_capacity(segments.len());
    let mut mask = Vec::new();
    let mut wordless_dropped = 0;
    for s in segments {
        let edits: Vec<&Segment> = edited.iter().filter(|e| e.track == s.track).collect();
        if s.words.is_empty() {
            let span = Word {
                text: String::new(),
                start: s.start,
                end: s.end,
                probability: 0.0,
            };
            if edits.iter().any(|e| under_edit(&span, e)) {
                wordless_dropped += 1;
            } else {
                kept_segments.push(s);
            }
            continue;
        }
        mask.extend(
            s.words
                .iter()
                .map(|w| edits.iter().any(|e| under_edit(w, e))),
        );
        kept_segments.push(s);
    }
    let (out, removed) = drop_masked(&kept_segments, &mask);
    (out, removed + wordless_dropped)
}

/// Whether a meeting that recorded these track kinds was recorded in the room: the mic and
/// no computer audio. The one rule for the pass and for marking a voice as the user
/// ([`crate::edit::records_in_room`]). Computer audio alone is the far end, and a mic with
/// computer audio is a call ([`two_track`]).
pub(crate) fn in_room_kinds(recorded: &BTreeSet<TrackId>) -> bool {
    recorded.contains(&TrackId::Mic) && !recorded.contains(&TrackId::Reference)
}

/// Names the diarised speakers against the stored voiceprints (module docs, step 4),
/// promoting the best user match among several mic voices to "Me" in `diar`.
fn name_speakers<Repo: VoiceprintRepo + ?Sized>(
    repo: &Repo,
    diar: &mut MeetingDiarization,
    model: &EmbeddingModelId,
    threshold: f32,
    warnings: &mut Vec<PostWarning>,
) -> Result<Vec<Naming>, RuntimeError> {
    let vp = RuntimeError::Voiceprint;
    // One read of the persons and this model's voiceprints for all speakers.
    let mut snapshot = InMemoryVoiceprintRepo::new();
    let persons = repo.persons().map_err(vp)?;
    for p in &persons {
        snapshot.put_person(p.clone()).map_err(vp)?;
    }
    for v in repo.voiceprints(model).map_err(vp)? {
        snapshot.put_voiceprint(v).map_err(vp)?;
    }
    let is_me = |id: EntityId| persons.iter().any(|p| p.id == id && p.is_me);

    let mut matches: Vec<Option<(EntityId, f32)>> = Vec::new();
    for s in &diar.speakers {
        if s.role == SpeakerRole::Me {
            matches.push(None);
            continue;
        }
        let report = match_embedding_with_report(&snapshot, &s.centroid, threshold).map_err(vp)?;
        if report.skipped_invalid > 0 {
            warnings.push(PostWarning::SkippedVoiceprints(report.skipped_invalid));
        }
        matches.push(match report.outcome {
            MatchOutcome::Known { person, score } => Some((person, score)),
            MatchOutcome::Unknown { .. } => None,
        });
    }
    // The best user match among the mic voices (several local voices of a call, or the
    // voices of an in-person recording on the mic) becomes "Me" (only if there is no Me
    // yet).
    let mic_voice = |s: &MeetingSpeaker| match s.role {
        SpeakerRole::Local => true,
        SpeakerRole::Room => s.track == TrackId::Mic,
        SpeakerRole::Me | SpeakerRole::Remote => false,
    };
    if diar.me().is_none() {
        let best = diar
            .speakers
            .iter()
            .zip(&matches)
            .enumerate()
            .filter(|(_, (s, m))| mic_voice(s) && m.is_some_and(|(p, _)| is_me(p)))
            .max_by(|a, b| {
                let score = |x: &(usize, (_, &Option<(EntityId, f32)>))| x.1.1.map_or(0.0, |m| m.1);
                score(a).total_cmp(&score(b))
            })
            .map(|(k, _)| k);
        if let Some(k) = best {
            diar.speakers[k].role = SpeakerRole::Me;
        }
    }
    let mut out = Vec::new();
    for (s, m) in diar.speakers.iter().zip(matches) {
        let naming = match (s.role, m) {
            (SpeakerRole::Me, _) => Naming {
                role: SpeakerRole::Me,
                person: None,
                name: None,
            },
            // Nobody else is the user: a far-end or second mic voice matching the user's
            // voiceprint (echo residue, a similar voice) stays unknown.
            (role, Some((p, _))) if !is_me(p) => Naming {
                role,
                person: Some(p),
                name: persons
                    .iter()
                    .find(|x| x.id == p)
                    .map(|x| x.display_name.clone()),
            },
            (role, _) => Naming {
                role,
                person: None,
                name: None,
            },
        };
        out.push(naming);
    }
    Ok(out)
}

/// `N` of an automatic speaker name "Speaker N".
fn speaker_number(name: &str) -> Option<usize> {
    name.strip_prefix("Speaker ")?.parse::<usize>().ok()
}

/// Words per speaker of `d` in `segments`, assigned exactly as the stored transcript will
/// be ([`label_segments`] over `d`'s turns).
fn words_per_speaker(d: &Diarization, segments: &[Segment]) -> Vec<usize> {
    let mut counts = vec![0; d.speakers.len()];
    for k in word_speakers(d, segments).into_iter().flatten() {
        counts[k] += 1;
    }
    counts
}

/// The speaker of `d` of every word of `segments` (in order over all their words), as
/// [`label_segments`] assigns it over `d`'s turns.
fn word_speakers(d: &Diarization, segments: &[Segment]) -> Vec<Option<usize>> {
    let tag = |k: usize| {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(k as u64).to_le_bytes());
        b[15] = 1;
        EntityId::from_bytes(b)
    };
    let mut n = 0u64;
    let mut new_id = || {
        n += 1;
        let mut b = [0xFFu8; 16];
        b[..8].copy_from_slice(&n.to_le_bytes());
        EntityId::from_bytes(b)
    };
    // label_segments keeps every word, in order (it only splits segments).
    let mut out = Vec::new();
    for s in label_segments(segments.to_vec(), &d.turns, |k| Some(tag(k)), &mut new_id) {
        let k = (0..d.speakers.len()).find(|&k| s.speaker_id == Some(tag(k)));
        out.extend(std::iter::repeat_n(k, s.words.len()));
    }
    out
}

/// A faint-echo voice is at least this far below the loudest mic voice (T9.1i; a judgement).
const QUIET_RESIDUE_DB: f64 = 10.0;
/// At least this share of its words lie where echo can be heard (a judgement).
const QUIET_RESIDUE_OVERLAP: f64 = 0.8;
/// It holds at most this share of the mic's words (a judgement).
const QUIET_RESIDUE_SHARE: f64 = 0.1;
/// A word this close to a far-end turn counts as heard with it (the speaker check's tolerance).
const QUIET_RESIDUE_TOLERANCE_MS: u64 = 1_000;
/// The frame a word's level is read over.
const LEVEL_FRAME_MS: usize = 30;

/// The loudest [`LEVEL_FRAME_MS`] of `word` in `audio`, in dBFS: a word reads by its loudest
/// frames, not by the quiet around it.
fn word_level_db(word: &Word, audio: &MonoBuffer) -> f64 {
    let per_ms = audio.rate.hz() as usize / 1000;
    let at = |ms: u64| {
        (usize::try_from(ms)
            .unwrap_or(usize::MAX)
            .saturating_mul(per_ms))
        .min(audio.samples.len())
    };
    let part = &audio.samples[at(word.start.as_millis())..at(word.end.as_millis())];
    part.chunks((LEVEL_FRAME_MS * per_ms).max(1))
        .map(|frame| {
            let power = frame
                .iter()
                .map(|s| f64::from(*s) * f64::from(*s))
                .sum::<f64>()
                / frame.len() as f64;
            if power > 1e-12 {
                10.0 * power.log10()
            } else {
                -120.0
            }
        })
        .fold(-120.0, f64::max)
}

/// Which mic voices are faint echo of the far end (T9.1i, [`drop_echo_clusters`] rule 3): quieter
/// than the loudest mic voice by [`QUIET_RESIDUE_DB`] (median word level), at least
/// [`QUIET_RESIDUE_OVERLAP`] of their words inside a cleaning stretch while the far end speaks,
/// and at most [`QUIET_RESIDUE_SHARE`] of the mic's words. All three: quiet alone is not echo (a
/// person across the room talks when the far end does not), and overlap alone is not either (the
/// user talks over people).
fn quiet_echo_residue(
    voices: usize,
    words: &[&Word],
    speakers: &[Option<usize>],
    reference: &Diarization,
    stretches: &[EchoLock],
    audio: &MonoBuffer,
) -> Vec<bool> {
    let mut levels: Vec<Vec<f64>> = vec![Vec::new(); voices];
    let mut echoing = vec![0usize; voices];
    let far_speaks = |mid: u64| {
        reference.turns.iter().any(|t| {
            t.start
                .as_millis()
                .saturating_sub(QUIET_RESIDUE_TOLERANCE_MS)
                <= mid
                && mid <= t.end.as_millis() + QUIET_RESIDUE_TOLERANCE_MS
        })
    };
    for (word, speaker) in words.iter().zip(speakers) {
        let Some(k) = *speaker else { continue };
        levels[k].push(word_level_db(word, audio));
        let mid = word.midpoint().as_millis();
        if stretches.iter().any(|s| s.covers(mid)) && far_speaks(mid) {
            echoing[k] += 1;
        }
    }
    let median = |v: &[f64]| {
        let mut sorted = v.to_vec();
        sorted.sort_by(f64::total_cmp);
        sorted.get(sorted.len() / 2).copied()
    };
    let medians: Vec<Option<f64>> = levels.iter().map(|v| median(v)).collect();
    let loudest = medians
        .iter()
        .flatten()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let total: usize = levels.iter().map(Vec::len).sum();
    (0..voices)
        .map(|k| {
            let count = levels[k].len();
            medians[k].is_some_and(|m| loudest - m >= QUIET_RESIDUE_DB)
                && echoing[k] as f64 >= QUIET_RESIDUE_OVERLAP * count as f64
                && count as f64 <= QUIET_RESIDUE_SHARE * total as f64
        })
        .collect()
}

/// What [`drop_echo_clusters`] decided.
#[derive(Debug, PartialEq)]
struct DroppedClusters {
    /// The speaker check's `me` with the remaining speakers' new indices.
    me: Option<Vec<usize>>,
    /// For each word of `after` (in order over all its words), whether it was kept by a
    /// rule-2 echo cluster: far-end words, removed and counted by the caller
    /// ([`EchoReport::cluster_removed`]).
    cluster_words: Vec<bool>,
}

/// Removes the echo clusters from the mic diarisation `d` (module docs, step 2), comparing
/// the words each speaker had before echo cleaning (`before`) with what it has after
/// (`after`), counted per speaker as the transcript is labelled ([`words_per_speaker`]).
/// A speaker is an echo cluster, never a "Local" voice or "Speaker N", when
///
/// 1. it had words and has none left, unless the speaker check found it to be the user
///    (`me`); or
/// 2. (T6.23a; rule defined after seeing the a private recording data, nothing numeric tuned)
///    all three hold: (a) echo cleaning removed more of its words than it kept (strictly
///    more than half: a midpoint, not a tuned value); (b) every word it kept has its
///    midpoint inside a cleaning stretch (`stretches`, so echo cleaning judged it; outside
///    every stretch nothing is attributed to echo); (c) its centroid's cosine with some
///    loopback speaker of `reference` is at or above `same_speaker` (the embedding
///    model's existing open-set voiceprint calibration, as in the speaker check). The
///    speaker check's user test does not exempt it: a cluster passes that test by talking
///    alone while the far end is silent, and is then judged against its own centroid.
///    Rule 2 never removes every mic voice that still has words: if it would, it removes
///    nothing. Rule 2 applies only when the speaker check ran (`me` is `Some`; a model
///    without a calibrated speaker-check margin gets neither). The words such a cluster
///    kept are returned in
///    [`DroppedClusters::cluster_words`] for removal (decided 2026-10-26 by the
///    coordinator, pending the owner). Known edge case: a user whose own voice comes back on
///    the loopback (a far end that echoes the user) matches a loopback speaker, so with a
///    second mic voice present and most of the user's words removed as echo, the user's
///    cluster would be dropped.
///
/// 3. (T9.1i; rule defined after seeing one real meeting) [`quiet_echo_residue`]: faint echo
///    in fragments too short for the text rule. Its words are removed as rule 2's are, and
///    it never removes every mic voice.
///
/// # Errors
/// [`DiarError::ModelMismatch`] if a mic and a loopback centroid come from different
/// embedding models.
#[allow(clippy::too_many_arguments)]
fn drop_echo_clusters(
    d: &mut Diarization,
    reference: Option<&Diarization>,
    same_speaker: f32,
    stretches: &[EchoLock],
    before: &[Segment],
    after: &[Segment],
    me: Option<&[usize]>,
    audio: Option<&MonoBuffer>,
) -> Result<DroppedClusters, DiarError> {
    let (had, has) = (words_per_speaker(d, before), words_per_speaker(d, after));
    let after_words: Vec<&Word> = after.iter().flat_map(|s| s.words.iter()).collect();
    let after_speakers = word_speakers(d, after);
    let is_me = |k: usize| me.is_some_and(|m| m.contains(&k));
    let mut echo: Vec<bool> = (0..d.speakers.len())
        .map(|k| had[k] > 0 && has[k] == 0 && !is_me(k))
        .collect();
    let mut sounds_far = vec![false; d.speakers.len()];
    // Rule 2 is part of the speaker check: only when it ran (`me` is its answer).
    if let (Some(r), Some(_)) = (reference, me) {
        for (k, s) in d.speakers.iter().enumerate() {
            for t in &r.speakers {
                if s.centroid.cosine_similarity(&t.centroid)? >= same_speaker {
                    sounds_far[k] = true;
                }
            }
        }
    }
    let mut outside = vec![0usize; d.speakers.len()];
    for (w, k) in after_words.iter().zip(&after_speakers) {
        let mid = w.midpoint().as_millis();
        if let Some(k) = k
            && !stretches.iter().any(|s| s.covers(mid))
        {
            outside[*k] += 1;
        }
    }
    let quiet = match (reference, audio) {
        (Some(r), Some(audio)) if !stretches.is_empty() => quiet_echo_residue(
            d.speakers.len(),
            &after_words,
            &after_speakers,
            r,
            stretches,
            audio,
        ),
        _ => vec![false; d.speakers.len()],
    };
    let mostly_echo: Vec<bool> = (0..d.speakers.len())
        .map(|k| {
            (has[k] > 0
                && had[k].saturating_sub(has[k]) > has[k]
                && outside[k] == 0
                && sounds_far[k])
                || quiet[k]
        })
        .collect();
    let voices_left = (0..d.speakers.len()).any(|k| has[k] > 0 && !echo[k] && !mostly_echo[k]);
    let rule2: Vec<bool> = mostly_echo.iter().map(|&m| m && voices_left).collect();
    for (e, m) in echo.iter_mut().zip(&rule2) {
        *e |= *m;
    }
    let cluster_words: Vec<bool> = after_speakers
        .iter()
        .map(|k| k.is_some_and(|k| rule2[k]))
        .collect();
    if !echo.contains(&true) {
        return Ok(DroppedClusters {
            me: me.map(<[usize]>::to_vec),
            cluster_words,
        });
    }
    let mut new_index = vec![None; d.speakers.len()];
    let mut next = 0;
    for (k, e) in echo.iter().enumerate() {
        if !e {
            new_index[k] = Some(next);
            next += 1;
        }
    }
    let speakers = std::mem::take(&mut d.speakers);
    d.speakers = speakers
        .into_iter()
        .zip(&echo)
        .filter(|(_, e)| !**e)
        .map(|(s, _)| s)
        .collect();
    let turns = std::mem::take(&mut d.turns);
    d.turns = turns
        .into_iter()
        .filter_map(|mut t| {
            t.speaker = new_index[t.speaker]?;
            Some(t)
        })
        .collect();
    Ok(DroppedClusters {
        me: me.map(|m| m.iter().filter_map(|&k| new_index[k]).collect()),
        cluster_words,
    })
}
/// Names the mic voice the speaker check found to be the user "Me", when it is the only
/// one (`me`: mic diarisation indices of the voices that passed the check's user test;
/// several means real voices in the room, and the check cannot tell which one is the user).
/// Returns whether it named one.
fn promote_checked_me(diar: &mut MeetingDiarization, namings: &mut [Naming], me: &[usize]) -> bool {
    let mic: Vec<usize> = diar
        .speakers
        .iter()
        .enumerate()
        .filter(|(_, s)| s.track == TrackId::Mic)
        .map(|(i, _)| i)
        .collect();
    let [only] = me else {
        return false;
    };
    let Some(&i) = mic.get(*only) else {
        return false;
    };
    // A voice the user's voiceprint did not match but someone else's did is that person.
    if namings[i].person.is_some() {
        return false;
    }
    diar.speakers[i].role = SpeakerRole::Me;
    namings[i] = Naming {
        role: SpeakerRole::Me,
        person: None,
        name: None,
    };
    true
}
impl<T, S, E> PostProcessor<T, S, E>
where
    T: Transcriber + Send + 'static,
    S: SpeakerSegmenter + Send + 'static,
    E: SpeakerEmbedder + Send + 'static,
{
    /// Runs [`PostProcessor::run`] on a new thread.
    ///
    /// # Errors
    /// [`RuntimeError::Spawn`] if the thread cannot be started.
    pub fn spawn(
        mut self,
        services: Services,
        input: PostInput,
        mut rng: BoxRng,
    ) -> Result<PostJob<T, S, E>, RuntimeError> {
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("earshot-post".into())
            .spawn(move || {
                let result = self.run(&services, input, &mut *rng, &token);
                (self, result)
            })
            .map_err(RuntimeError::Spawn)?;
        Ok(PostJob { cancel, thread })
    }
}
