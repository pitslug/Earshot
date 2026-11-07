//! Live transcription of one track ([`LiveTrack`]): a backlog of pipeline audio drained
//! into one of two engines. The pipeline runs one inside [`super::Pipeline::poll`] when
//! live ASR is attached; a host may instead own one per track and run it on a thread of
//! its own, fed with the pipeline's `Audio` events (the runtime does, T6.10a).
//!
//! Engines: a [`StreamingChunker`] ([`LiveTrack::new`]: partials and finals per VAD
//! utterance), or speech stretches ([`LiveTrack::with_stretches`], T6.13b): a
//! [`PieceStream`] decodes each speech piece once as soon as it is settled and emits its
//! words as finals, with no partials; the result equals the whole-track pass on the same
//! audio (D20).
//!
//! States: **live** (audio is queued and decoded at each poll) or **stopped** (the engine
//! failed or was cancelled: it is dropped, the backlog is cleared and no more audio is
//! queued). Stopping is reported once as [`PipelineError::LiveAsr`].
//!
//! The stretches also keep what they decoded, before any host filtering, for the final
//! pass to reuse (T6.13c): [`LiveTrack::take_decoded_pieces`] hands over each decoded
//! piece once, and [`LiveTrack::stretches_through`] is the stream's decoded frontier after
//! the last of them (kept when the engine stops, so a stopped track's coverage is known).
//! A piece is kept, with its frontier, and its final queued as soon as its own decode
//! returns ([`PieceStream::push_each`]), so when a later decode of the same
//! [`LiveTrack::run`] or [`LiveTrack::finish`] fails, or panics (a host catching the
//! unwind still owns the track), the pieces before it are kept and shown.
//! The final pass of a stretch track decodes with the stretches' own options
//! ([`LiveTrack::final_pass`]), so audio no stretch covers is transcribed alike.

use std::fmt;
use std::mem;

use super::COALESCE_TAIL;
use super::types::{FinalPassTrack, LiveAsr, PartialPolicy, PipelineError, PipelineEvent};
use crate::asr::{
    AsrError, CancelToken, MeetingTime, PieceConfig, PieceStream, SegmentIds, StreamPiece,
    StreamingChunker, StreamingTranscriber, TranscribeOptions, Transcriber, TranscriptEvent,
    Transcription, Vad,
};
use crate::audio::{PIPELINE_RATE, Sample};
use crate::capture::TrackId;
use crate::identity::EntityId;

/// Queues `out` as transcript events of `track`.
fn forward(track: TrackId, out: Vec<TranscriptEvent>, events: &mut Vec<PipelineEvent>) {
    events.extend(
        out.into_iter()
            .map(|event| PipelineEvent::Transcript { track, event }),
    );
}

type Chunker = StreamingChunker<Box<dyn Transcriber + Send>, Box<dyn Vad + Send>>;

/// A placeholder id for the model segments a piece's final is built from (never used).
const CONTEXT_FREE: EntityId = EntityId::from_bytes([0; 16]);

/// The stretch engine (module docs).
struct Stretches {
    transcriber: Box<dyn Transcriber + Send>,
    vad: Box<dyn Vad + Send>,
    /// `None` after `finish`.
    stream: Option<PieceStream>,
    segment_ids: SegmentIds,
    cancel: CancelToken,
}

/// A decoded piece as one transcript final holding all its words in order (non-speech
/// annotations dropped, as `Transcription::into_segments` does); a piece left without
/// words gives none. One final per piece, not per model segment: a piece is decided the
/// same way however the audio arrived (`asr::PieceStream`), so the live echo rule, which
/// looks for runs of repeated words within and across finals, sees the same units on
/// every run.
fn piece_final(
    entity: EntityId,
    backend: &crate::asr::BackendInfo,
    segment_ids: &mut SegmentIds,
    piece: &StreamPiece,
) -> Option<TranscriptEvent> {
    let t = Transcription {
        segments: piece.segments.clone(),
        language: None,
        backend: backend.clone(),
    };
    let mut parts = t.into_segments(entity, MeetingTime::ZERO, &mut || CONTEXT_FREE);
    let start = parts.first()?.start;
    let words = parts.drain(..).flat_map(|s| s.words).collect();
    let id = segment_ids();
    Some(TranscriptEvent::Final(crate::asr::Segment::from_words(
        id, entity, words, start,
    )))
}

/// Where a stretch engine's pieces go as each is decoded (module docs): kept for
/// [`LiveTrack::take_decoded_pieces`] with the frontier after it, and queued as a final.
struct PieceLog<'a> {
    track: TrackId,
    entity: EntityId,
    backend: crate::asr::BackendInfo,
    segment_ids: &'a mut SegmentIds,
    decoded: &'a mut Vec<StreamPiece>,
    through: &'a mut Option<u64>,
    events: &'a mut Vec<PipelineEvent>,
}

impl PieceLog<'_> {
    fn add(&mut self, piece: StreamPiece, frontier: usize) {
        *self.through = Some(frontier as u64);
        if let Some(event) = piece_final(self.entity, &self.backend, self.segment_ids, &piece) {
            self.events.push(PipelineEvent::Transcript {
                track: self.track,
                event,
            });
        }
        self.decoded.push(piece);
    }
}

enum Engine {
    Chunker(Box<Chunker>),
    Stretches(Box<Stretches>),
}

/// Live ASR of one track (module docs). Feed it the track's pipeline audio in order; run
/// it to decode; finish it at the end.
pub struct LiveTrack {
    track: TrackId,
    entity: EntityId,
    /// `None` once stopped.
    engine: Option<Engine>,
    /// Pipeline audio not yet pushed into the engine.
    backlog: Vec<Sample>,
    /// Samples pushed into the engine.
    fed: u64,
    /// Samples given to [`LiveTrack::feed`], decoded or not.
    received: u64,
    /// Final-pass decoding: [`TranscribeOptions::batch`] for the chunker, the stretches'
    /// own options for the stretches (module docs).
    final_options: TranscribeOptions,
    /// The stretches' decoded pieces not yet taken ([`LiveTrack::take_decoded_pieces`]).
    decoded: Vec<StreamPiece>,
    /// The stretches' decoded frontier after the last decoded piece; `None` for the
    /// chunker.
    stretches_through: Option<u64>,
    /// The backend the stretches' transcriber reported after its last engine call (a
    /// backend that falls back per call reports where it actually ran); `None` for the
    /// chunker.
    stretches_backend: Option<crate::asr::BackendInfo>,
}

impl fmt::Debug for LiveTrack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveTrack")
            .field("track", &self.track)
            .field("entity", &self.entity)
            .field("live", &self.engine.is_some())
            .field("backlog", &self.backlog.len())
            .finish()
    }
}

impl LiveTrack {
    /// A chunker for `track` whose first sample is pipeline frame 0.
    ///
    /// # Errors
    /// [`PipelineError::LiveAsr`] for an invalid chunker config.
    pub fn new(track: TrackId, asr: LiveAsr) -> Result<Self, PipelineError> {
        let language = asr.config.options.language.clone();
        let chunker = StreamingChunker::new(
            asr.transcriber,
            asr.vad,
            asr.config,
            asr.track_entity,
            MeetingTime::ZERO,
            asr.segment_ids,
        )
        .map_err(|error| PipelineError::LiveAsr { track, error })?;
        Ok(Self {
            track,
            entity: asr.track_entity,
            final_options: TranscribeOptions {
                language,
                ..TranscribeOptions::batch()
            },
            engine: Some(Engine::Chunker(Box::new(chunker))),
            backlog: Vec::new(),
            fed: 0,
            received: 0,
            decoded: Vec::new(),
            stretches_through: None,
            stretches_backend: None,
        })
    }

    /// Speech stretches for `track` (module docs), decoded with `asr`'s transcriber, VAD and
    /// decoding options (`asr.config.options`; the rest of the chunker config is unused) in
    /// pieces planned by `pieces`.
    ///
    /// # Errors
    /// [`PipelineError::LiveAsr`] for an invalid piece config or VAD.
    pub fn with_stretches(
        track: TrackId,
        asr: LiveAsr,
        pieces: &PieceConfig,
    ) -> Result<Self, PipelineError> {
        let options = asr.config.options.clone();
        let backend = asr.transcriber.backend_info();
        let mut vad = asr.vad;
        let stream = PieceStream::new(&*asr.transcriber, &mut *vad, &options, pieces)
            .map_err(|error| PipelineError::LiveAsr { track, error })?;
        Ok(Self {
            track,
            entity: asr.track_entity,
            engine: Some(Engine::Stretches(Box::new(Stretches {
                transcriber: asr.transcriber,
                vad,
                stream: Some(stream),
                segment_ids: asr.segment_ids,
                cancel: CancelToken::new(),
            }))),
            backlog: Vec::new(),
            fed: 0,
            received: 0,
            final_options: options,
            decoded: Vec::new(),
            stretches_through: Some(0),
            stretches_backend: Some(backend),
        })
    }

    /// The stretches' decoded pieces since the last call, each exactly as decoded (stream
    /// time, before any echo or host filtering); empty for the chunker.
    pub fn take_decoded_pieces(&mut self) -> Vec<StreamPiece> {
        mem::take(&mut self.decoded)
    }

    /// The stretches' decoded frontier (pipeline frames) after the last decoded piece:
    /// every speech piece before it is among the pieces handed out by
    /// [`LiveTrack::take_decoded_pieces`]. Kept after the engine stops; `None` for the
    /// chunker.
    #[must_use]
    pub fn stretches_through(&self) -> Option<u64> {
        self.stretches_through
    }

    /// The backend the stretches' transcriber reported after its last engine call (at the
    /// start before any): the one that decoded the latest pieces. Kept after the engine
    /// stops; `None` for the chunker.
    #[must_use]
    pub fn stretches_backend(&self) -> Option<&crate::asr::BackendInfo> {
        self.stretches_backend.as_ref()
    }

    /// Whether the engine decodes partial hypotheses (the chunker) or finals only (the
    /// stretches).
    #[must_use]
    pub fn has_partials(&self) -> bool {
        matches!(self.engine, Some(Engine::Chunker(_)))
    }

    /// Pipeline frames up to which the track's speech is transcribed: everything pushed for
    /// the chunker, the decoded frontier for the stretches (a settled piece's audio waits in
    /// the stream until it is decoded); everything received once stopped (nothing more will
    /// come).
    #[must_use]
    pub fn decoded_through(&self) -> u64 {
        match &self.engine {
            None => self.received,
            Some(Engine::Chunker(_)) => self.fed,
            Some(Engine::Stretches(s)) => s
                .stream
                .as_ref()
                .map_or(self.fed, |st| st.decoded_through() as u64),
        }
    }

    /// Queues new pipeline audio of the track (contiguous with the previous call). If the
    /// queue would exceed `cap` samples, live ASR of the track stops ("fell behind").
    pub fn feed(&mut self, samples: &[Sample], cap: u64, events: &mut Vec<PipelineEvent>) {
        self.received += samples.len() as u64;
        if self.engine.is_none() {
            return;
        }
        if (self.backlog.len() + samples.len()) as u64 > cap {
            let secs = cap as f64 / f64::from(PIPELINE_RATE.hz());
            let error = AsrError::Backend(format!(
                "live ASR fell behind by more than {secs:.0} s of audio; the final pass will transcribe it"
            ));
            self.stop(error, events);
            return;
        }
        self.backlog.extend_from_slice(samples);
    }

    /// Samples queued.
    #[must_use]
    pub fn backlog(&self) -> u64 {
        self.backlog.len() as u64
    }

    /// The engine's cancel token, while live.
    #[must_use]
    pub fn cancel_token(&self) -> Option<CancelToken> {
        match self.engine.as_ref()? {
            Engine::Chunker(c) => Some(c.cancel_token()),
            Engine::Stretches(s) => Some(s.cancel.clone()),
        }
    }

    /// Decodes the backlog under `policy` (the chunker; the stretches have no partials),
    /// queueing transcript events.
    pub fn run(&mut self, policy: PartialPolicy, events: &mut Vec<PipelineEvent>) {
        let backlog = mem::take(&mut self.backlog);
        self.fed += backlog.len() as u64;
        let chunker = match self.engine.as_mut() {
            None => return,
            Some(Engine::Stretches(s)) => {
                let Stretches {
                    transcriber,
                    vad,
                    stream,
                    segment_ids,
                    cancel,
                } = &mut **s;
                let Some(st) = stream.as_mut() else {
                    return;
                };
                // Each piece is kept and shown as its decode returns (module docs).
                let mut log = PieceLog {
                    track: self.track,
                    entity: self.entity,
                    backend: transcriber.backend_info(),
                    segment_ids,
                    decoded: &mut self.decoded,
                    through: &mut self.stretches_through,
                    events,
                };
                let result = st.push_each(transcriber, vad, &backlog, cancel, &mut |p, f| {
                    log.add(p, f)
                });
                self.stretches_backend = Some(transcriber.backend_info());
                match result {
                    Ok(()) => self.stretches_through = Some(st.decoded_through() as u64),
                    Err(e) => self.stop(e, events),
                }
                return;
            }
            Some(Engine::Chunker(c)) => c,
        };
        let tail = PIPELINE_RATE.frames_for(COALESCE_TAIL);
        let split = match policy {
            PartialPolicy::All => 0,
            PartialPolicy::Coalesce => backlog.len().saturating_sub(tail),
            PartialPolicy::Off => backlog.len(),
        };
        let (head, rest) = backlog.split_at(split);
        let mut result = Ok(());
        for (part, partials) in [(head, false), (rest, policy != PartialPolicy::Off)] {
            if part.is_empty() {
                continue;
            }
            chunker.set_partial_decodes(partials);
            match chunker.push(part) {
                Ok(out) => forward(self.track, out, events),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if let Err(e) = result {
            self.stop(e, events);
        }
    }

    /// Decodes the backlog, then closes every open segment (the stretches: decodes the
    /// last pieces).
    pub fn finish(&mut self, policy: PartialPolicy, events: &mut Vec<PipelineEvent>) {
        self.run(policy, events);
        match self.engine.as_mut() {
            None => {}
            Some(Engine::Chunker(chunker)) => match chunker.finish() {
                Ok(out) => forward(self.track, out, events),
                Err(e) => self.stop(e, events),
            },
            Some(Engine::Stretches(s)) => {
                let Stretches {
                    transcriber,
                    vad,
                    stream,
                    segment_ids,
                    cancel,
                } = &mut **s;
                let Some(stream) = stream.take() else {
                    return;
                };
                let mut log = PieceLog {
                    track: self.track,
                    entity: self.entity,
                    backend: transcriber.backend_info(),
                    segment_ids,
                    decoded: &mut self.decoded,
                    through: &mut self.stretches_through,
                    events,
                };
                let result =
                    stream.finish_each(transcriber, vad, cancel, &mut |p, f| log.add(p, f));
                self.stretches_backend = Some(transcriber.backend_info());
                match result {
                    // The whole stream is decoded.
                    Ok(_) => self.stretches_through = Some(self.fed),
                    Err(e) => self.stop(e, events),
                }
            }
        }
    }

    /// Stops live ASR of the track (the engine's state is unspecified after an error).
    fn stop(&mut self, error: AsrError, events: &mut Vec<PipelineEvent>) {
        self.engine = None;
        self.backlog = Vec::new();
        events.push(PipelineEvent::Error(PipelineError::LiveAsr {
            track: self.track,
            error,
        }));
    }

    /// The final-pass entry of this track: [`TranscribeOptions::batch`] in the live language
    /// for the chunker; for the stretches their own options (greedy during the meeting,
    /// D20), so the final pass can reuse the stretches and decodes what they do not cover
    /// in the same way (module docs).
    #[must_use]
    pub fn final_pass(&self) -> FinalPassTrack {
        FinalPassTrack {
            track: self.track,
            track_entity: Some(self.entity),
            options: self.final_options.clone(),
        }
    }
}
