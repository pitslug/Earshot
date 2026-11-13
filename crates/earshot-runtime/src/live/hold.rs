//! Live work that starts after capture (T9.1l, [`super::LiveConfig::hold`]).
//!
//! A session with a [`LiveHold`] opens the source and the FLAC writers and commits its rows first;
//! nothing about models can cost it the recording. Its live speaker work and transcription start
//! later, each when its gate opens ([`HoldGates`]), and neither ever runs on the session thread:
//!
//! - A thread of its own (`earshot-live-hold`) waits at the gates and loads: speaker models first,
//!   then, once the meeting's turn has come, live transcription. It sends what it loaded to the
//!   session, which picks it up between two pumps. A model load blocks only this thread.
//! - Until then the session keeps each track's pipeline audio from frame 0 ([`HeldAudio`]) and
//!   hands it over in order when the work starts, so the live words are those of a run with no
//!   hold: stretch decoding does not depend on when its audio arrives.
//! - The bounds: past [`LiveHold::max_held`] (30 minutes, as for a stretch worker) the live
//!   transcript stops for the meeting; past [`LiveHold::max_speaker_buffer`] (as for a running
//!   speaker thread) live speaker work does. Each frees its queue and leaves the final pass more to
//!   do. The recording never depends on either.
//! - A failed load stops that work the same way: a warning, no record, the final pass does it whole.
//!   Stopping the meeting during a hold abandons the work that has not started: the recording is
//!   saved at once and the final pass transcribes the whole meeting in its turn. A source that runs
//!   out by itself (a replay, [`EndReason::Finished`]) has nobody waiting for it, and its hold runs
//!   to the end, so a replay faster than a model load still has its live pass.
//! - A load still in progress at the end is waited for, up to [`LiveHold::load_stall`] with
//!   nothing from the thread or until the catch-up is cancelled; then the thread is left, never
//!   joined, so a load that never returns cannot keep the session from ending (T9.22b).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use earshot_core::asr::{CancelToken, TranscribeOptions};
use earshot_core::audio::{PIPELINE_RATE, Sample};
use earshot_core::capture::{AudioSource, EndReason, TrackId};
use earshot_core::identity::EntityId;
use earshot_core::model::TranscriptPass;
use earshot_core::pipeline::FinalPassTrack;

use super::speaker_thread::{SpeakerThread, SpeakerTrack};
use super::summary_thread::SummaryThread;
use super::worker::{LiveWorker, StallWatch};
use super::{
    BuiltTrack, LiveAsrFactory, LiveSpeakers, LiveTranscription, RuntimeEvent, RuntimeWarning,
    SPEAKER_MAX_BUFFER, STRETCH_MAX_BACKLOG, Session, build_track, slot,
};
use crate::RuntimeError;

/// The backlog under which a catch-up is over: decoding is then keeping up with capture. A
/// judgement: long enough that one decode in progress does not read as falling behind.
pub const CAUGHT_UP_BACKLOG: Duration = Duration::from_secs(5);

/// What a hold leaves free of the speaker thread's own bound: the thread counts what it is handed,
/// and a hold of the whole `SPEAKER_MAX_BUFFER` would put it over with the next packet. A judgement:
/// time for the first batches to go through.
pub const SPEAKER_HANDOVER_MARGIN: Duration = Duration::from_secs(30);

/// How long the end of a held session waits for a model load in progress on the hold thread
/// before it leaves the thread ([`LiveHold::load_stall`], T9.22b). A judgement, the same as a GPU
/// summary load's (`summary::GPU_LOAD_LIMIT`): a load reports nothing until it returns and may
/// first read gigabytes from a slow disk, so only a load that never returns goes this long.
pub const HOLD_LOAD_STALL: Duration = Duration::from_secs(600);

/// How often the end of a held session looks at the hold thread.
const HOLD_WATCH_POLL: Duration = Duration::from_millis(50);

/// When a held session's live work may start. Implementations block, and return `false` as soon as
/// `cancel` is set (the session is ending, or no longer wants the work).
pub trait HoldGates: Send + Sync {
    /// Waits until live speaker work may start.
    fn wait_speakers(&self, cancel: &CancelToken) -> bool;
    /// Waits until live transcription may start (the meeting's turn).
    fn wait_transcription(&self, cancel: &CancelToken) -> bool;
    /// What the session is waiting for now; `None`: nothing, it only has to load its models.
    fn reason(&self) -> Option<HoldReason>;
}

/// Why a session's live transcription is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// An older meeting's final pass or summary is running.
    PreviousMeeting,
    /// The app is still deciding what its models run on.
    Starting,
}

/// Where a held session's live transcript is, for a banner. Only sent to change it: a session that
/// never held anything and never fell behind sends none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveTranscriptState {
    /// The audio is recorded; the transcript waits.
    Held(HoldReason),
    /// The transcript is working through the audio queued so far.
    CatchingUp,
    /// The backlog has cleared.
    UpToDate,
    /// The live transcript stopped for this meeting; the final pass does the rest. Never changes
    /// again.
    Stopped,
}

/// Speaker models of a held session, one track at a time; runs on the hold thread after
/// [`HoldGates::wait_speakers`].
pub type HeldSpeakerFactory =
    Arc<dyn Fn(TrackId) -> Result<Option<LiveSpeakers>, RuntimeError> + Send + Sync>;

/// Warnings from a hold's loads ([`LiveHold::notes`]).
pub type HoldNotes = Arc<Mutex<Vec<RuntimeWarning>>>;

/// How a session starts its live work after capture (module docs).
#[derive(Clone)]
pub struct LiveHold {
    /// When each part may start.
    pub gates: Arc<dyn HoldGates>,
    /// Speaker work (`None`: none during the meeting). A held session's speaker work comes only
    /// from here, so that it can start before transcription; a [`super::TrackAsr`] that brings
    /// its own speaker models is a failed load.
    pub speakers: Option<HeldSpeakerFactory>,
    /// Warnings from the hold's loads, sent by the session when it next looks.
    pub notes: HoldNotes,
    /// The most audio a track holds for its live transcript.
    pub max_held: Duration,
    /// The most audio a track holds for speaker work that has not started.
    pub max_speaker_buffer: Duration,
    /// At the end, a hold thread that has not finished this long after it was told to stop (a
    /// model load in progress, which polls nothing) is left, never joined, and
    /// [`super::RuntimeWarning::LiveLoadStalled`] says so (T9.22b). Each message from the thread
    /// (a load that returned) starts the bound again. [`HOLD_LOAD_STALL`] in production.
    pub load_stall: Duration,
}

impl LiveHold {
    /// A hold on `gates` with the production bounds, without speaker work.
    #[must_use]
    pub fn new(gates: Arc<dyn HoldGates>) -> Self {
        Self {
            gates,
            speakers: None,
            notes: Arc::default(),
            max_held: STRETCH_MAX_BACKLOG,
            max_speaker_buffer: SPEAKER_MAX_BUFFER.saturating_sub(SPEAKER_HANDOVER_MARGIN),
            load_stall: HOLD_LOAD_STALL,
        }
    }
}

impl std::fmt::Debug for LiveHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveHold")
            .field("speakers", &self.speakers.is_some())
            .field("max_held", &self.max_held)
            .field("max_speaker_buffer", &self.max_speaker_buffer)
            .field("load_stall", &self.load_stall)
            .finish_non_exhaustive()
    }
}

impl PartialEq for LiveHold {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(Arc::as_ptr(&self.gates), Arc::as_ptr(&other.gates))
    }
}

/// What the final pass of a held track reads until its live work starts, and for good if that never
/// happens: the stretches' own decoding (D20), the track's row.
pub(super) fn held_entry(
    track: TrackId,
    entity: EntityId,
    transcription: &LiveTranscription,
) -> FinalPassTrack {
    FinalPassTrack {
        track,
        track_entity: Some(entity),
        options: match transcription {
            LiveTranscription::Stretches(_) => earshot_core::asr::ChunkerConfig::default().options,
            LiveTranscription::Chunker => TranscribeOptions::batch(),
        },
    }
}

/// A track's queued audio. A chunk held for transcription and for speaker work is one
/// allocation shared by both queues (TODO 50: two copies cost about 230 MB at the cap).
#[derive(Debug, Default)]
struct TrackQueue {
    chunks: Vec<Arc<Vec<Sample>>>,
    frames: u64,
}

/// Audio kept for work that has not started, per track (by [`slot`]).
#[derive(Debug, Default)]
pub(super) struct HeldAudio {
    tracks: [TrackQueue; 2],
    /// The queues' lengths, for the pipeline's stats.
    gauge: [Arc<AtomicU64>; 2],
}

impl HeldAudio {
    /// Queues `samples` (shared with the other queue that holds them); the track's length after
    /// it.
    fn push(&mut self, track: TrackId, samples: Arc<Vec<Sample>>) -> u64 {
        let q = &mut self.tracks[slot(track)];
        q.frames += samples.len() as u64;
        q.chunks.push(samples);
        self.gauge[slot(track)].store(q.frames, Ordering::SeqCst);
        q.frames
    }

    /// Takes `track`'s queue, frames in order.
    fn take(&mut self, track: TrackId) -> (Vec<Arc<Vec<Sample>>>, u64) {
        let q = std::mem::take(&mut self.tracks[slot(track)]);
        self.gauge[slot(track)].store(0, Ordering::SeqCst);
        (q.chunks, q.frames)
    }

    fn clear(&mut self) {
        for track in [TrackId::Mic, TrackId::Reference] {
            let _ = self.take(track);
        }
    }

    /// `track`'s length, for the pipeline's stats while the queue holds it.
    pub(super) fn backlog_source(&self, track: TrackId) -> Box<dyn Fn() -> u64 + Send> {
        let gauge = Arc::clone(&self.gauge[slot(track)]);
        Box::new(move || gauge.load(Ordering::SeqCst))
    }
}

/// What the hold thread sends the session.
enum FromHold {
    Speakers(Result<Vec<SpeakerTrack>, RuntimeError>),
    Transcription(Result<Vec<BuiltTrack>, RuntimeError>),
}

/// What the hold thread needs.
struct HoldWork {
    gates: Arc<dyn HoldGates>,
    speakers: Option<HeldSpeakerFactory>,
    tracks: Vec<(TrackId, EntityId)>,
    factory: LiveAsrFactory,
    transcription: LiveTranscription,
    cancel: CancelToken,
    speaker_cancel: CancelToken,
    out: Sender<FromHold>,
}

impl HoldWork {
    fn run(mut self) {
        if let Some(speakers) = self.speakers.take()
            && self.gates.wait_speakers(&self.speaker_cancel)
        {
            let loaded = self.load_speakers(&*speakers);
            let _ = self.out.send(FromHold::Speakers(loaded));
        }
        if self.gates.wait_transcription(&self.cancel) {
            let built = self.build_tracks();
            let _ = self.out.send(FromHold::Transcription(built));
        }
    }

    fn load_speakers(
        &self,
        speakers: &(dyn Fn(TrackId) -> Result<Option<LiveSpeakers>, RuntimeError> + Send + Sync),
    ) -> Result<Vec<SpeakerTrack>, RuntimeError> {
        let mut out = Vec::new();
        for (track, _) in &self.tracks {
            let Some(s) = speakers(*track)? else {
                continue;
            };
            out.push(SpeakerTrack {
                track: *track,
                diarizer: (s.models)()?,
                threads: s.threads,
                progress: s.progress,
                background: s.background,
            });
        }
        Ok(out)
    }

    fn build_tracks(&mut self) -> Result<Vec<BuiltTrack>, RuntimeError> {
        let mut out = Vec::new();
        for (track, entity) in &self.tracks {
            let Some(asr) = (self.factory)(*track, *entity)? else {
                continue;
            };
            if asr.speakers.is_some() {
                return Err(RuntimeError::Internal(
                    "a held session's speaker work comes from its LiveHold",
                ));
            }
            out.push(build_track(*track, asr, &self.transcription)?);
        }
        Ok(out)
    }
}

/// How the wait for a stopped hold thread ended ([`Session::end_hold`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldThreadEnd {
    /// It ended.
    Ended,
    /// The catch-up was cancelled first.
    Cancelled,
    /// Nothing came from it for the bound: a load that does not return.
    Stalled,
}

/// Waits for the hold thread, already told to stop, to end: its sender goes when it does. What it
/// still sends (a load that returned) is dropped and counts as a sign of life for the bound
/// ([`StallWatch`], the live catch-up's rule).
fn wait_for_hold_thread(
    from: &Receiver<FromHold>,
    bound: Duration,
    cancel: &CancelToken,
) -> HoldThreadEnd {
    let mut watch = StallWatch::new(bound);
    let started = Instant::now();
    let mut messages = 0u64;
    loop {
        match from.recv_timeout(HOLD_WATCH_POLL) {
            Ok(_) => messages += 1,
            Err(RecvTimeoutError::Disconnected) => return HoldThreadEnd::Ended,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if cancel.is_cancelled() {
            return HoldThreadEnd::Cancelled;
        }
        if watch.stalled([messages, 0, 0], started.elapsed()) {
            return HoldThreadEnd::Stalled;
        }
    }
}

/// A session's hold while any of its work has not started (module docs).
pub(super) struct HoldState {
    hold: LiveHold,
    /// What the hold thread will run, until it is spawned.
    work: Option<HoldWork>,
    from: Receiver<FromHold>,
    cancel: CancelToken,
    speaker_cancel: CancelToken,
    thread: Option<JoinHandle<()>>,
    transcript_audio: HeldAudio,
    speaker_audio: HeldAudio,
    /// Transcription has not started and may still.
    pub(super) transcript_held: bool,
    /// Speaker work has not started and may still.
    speakers_held: bool,
    /// The session watches the power supply once work runs ([`Session::check_power`]).
    pub(super) watch_power: bool,
    max_held: u64,
    max_speaker_buffer: u64,
}

impl HoldState {
    /// The hold of a session with `tracks`, its transcription from `factory`, and speaker work
    /// only if `speakers_on`.
    pub(super) fn new(
        hold: LiveHold,
        factory: LiveAsrFactory,
        tracks: Vec<(TrackId, EntityId)>,
        transcription: LiveTranscription,
        speakers_on: bool,
    ) -> Self {
        let (out, from) = channel();
        let (cancel, speaker_cancel) = (CancelToken::new(), CancelToken::new());
        let speakers = hold.speakers.clone().filter(|_| speakers_on);
        let speakers_held = speakers.is_some();
        let frames = |d: Duration| PIPELINE_RATE.frames_for(d) as u64;
        Self {
            max_held: frames(hold.max_held),
            max_speaker_buffer: frames(hold.max_speaker_buffer),
            work: Some(HoldWork {
                gates: Arc::clone(&hold.gates),
                speakers,
                tracks,
                factory,
                transcription,
                cancel: cancel.clone(),
                speaker_cancel: speaker_cancel.clone(),
                out,
            }),
            hold,
            from,
            cancel,
            speaker_cancel,
            thread: None,
            transcript_audio: HeldAudio::default(),
            speaker_audio: HeldAudio::default(),
            transcript_held: true,
            speakers_held,
            watch_power: false,
        }
    }

    /// The queue's length as a backlog source for the pipeline's stats.
    pub(super) fn backlog_source(&self, track: TrackId) -> Box<dyn Fn() -> u64 + Send> {
        self.transcript_audio.backlog_source(track)
    }

    fn take_notes(&self) -> Vec<RuntimeWarning> {
        std::mem::take(
            &mut *self
                .hold
                .notes
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl Drop for HoldState {
    fn drop(&mut self) {
        // A session that went down without ending its hold: nothing is left to wait at the gates.
        self.cancel.cancel();
        self.speaker_cancel.cancel();
    }
}

impl<S> Session<S>
where
    S: AudioSource,
    S::Error: std::fmt::Debug,
{
    /// Starts the hold thread and says what the transcript waits for.
    pub(super) fn begin_hold(&mut self) {
        let Some(hold) = &mut self.hold else {
            return;
        };
        let reason = hold.hold.gates.reason();
        let Some(work) = hold.work.take() else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("earshot-live-hold".into())
            .spawn(move || work.run());
        match spawned {
            Ok(thread) => hold.thread = Some(thread),
            Err(e) => {
                self.load_failed(&e.to_string());
                return;
            }
        }
        if let Some(reason) = reason {
            self.set_transcript_state(LiveTranscriptState::Held(reason));
        }
    }

    /// Keeps `samples` for the work that has not started; past a bound, that work stops.
    pub(super) fn hold_audio(&mut self, track: TrackId, samples: &[Sample]) {
        let Some(hold) = &mut self.hold else {
            return;
        };
        if !hold.transcript_held && !hold.speakers_held {
            return;
        }
        let chunk = Arc::new(samples.to_vec());
        let mut transcript_over = false;
        if hold.transcript_held {
            transcript_over = hold.transcript_audio.push(track, Arc::clone(&chunk)) > hold.max_held;
        }
        let mut speakers_over = false;
        if hold.speakers_held {
            speakers_over = hold.speaker_audio.push(track, chunk) > hold.max_speaker_buffer;
        }
        if speakers_over {
            let seconds = hold.hold.max_speaker_buffer.as_secs();
            self.speakers_stopped(format!(
                "live speaker work was held for more than {seconds} s"
            ));
        }
        if transcript_over {
            self.transcript_stopped();
        }
    }

    /// Whether `track`'s transcription is still held (its audio goes to [`Self::hold_audio`]).
    pub(super) fn holding(&self) -> bool {
        self.hold
            .as_ref()
            .is_some_and(|h| h.transcript_held || h.speakers_held)
    }

    /// Takes up what the hold thread has sent, between two pumps: never waits.
    pub(super) fn check_hold(&mut self) {
        let Some(hold) = &self.hold else {
            return;
        };
        for note in hold.take_notes() {
            self.warn(note);
        }
        loop {
            let Some(hold) = &self.hold else {
                return;
            };
            match hold.from.try_recv() {
                Ok(message) => self.take_up(message),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.hold_thread_ended();
                    return;
                }
            }
        }
    }

    fn take_up(&mut self, message: FromHold) {
        match message {
            FromHold::Speakers(loaded) => self.speakers_loaded(loaded),
            FromHold::Transcription(built) => self.transcription_loaded(built),
        }
    }

    /// The thread ended. With work still held it crashed: that work stops.
    fn hold_thread_ended(&mut self) {
        if self.hold.as_ref().is_some_and(|h| h.transcript_held) {
            self.load_failed("the live hold thread stopped");
        }
        if self.hold.as_ref().is_some_and(|h| h.speakers_held) {
            self.speakers_stopped("the live hold thread stopped".to_owned());
        }
        self.hold = None;
    }

    fn speakers_loaded(&mut self, loaded: Result<Vec<SpeakerTrack>, RuntimeError>) {
        let Some(hold) = &mut self.hold else {
            return;
        };
        if !hold.speakers_held {
            return;
        }
        let loaded = loaded.and_then(|tracks| {
            let threads: Vec<(TrackId, Option<usize>)> =
                tracks.iter().map(|t| (t.track, t.threads)).collect();
            Ok((threads, SpeakerThread::spawn(tracks)?))
        });
        match loaded {
            Ok((threads, thread)) => {
                hold.speakers_held = false;
                let watch_power = hold.watch_power;
                for track in [TrackId::Mic, TrackId::Reference] {
                    let (chunks, _) = hold.speaker_audio.take(track);
                    for chunk in chunks {
                        thread.send(track, &chunk);
                    }
                }
                self.speaker_threads = threads;
                self.speakers = Some(thread);
                if watch_power {
                    self.next_power_read.get_or_insert_with(Instant::now);
                }
            }
            Err(e) => self.speakers_stopped(format!("the speaker models did not load: {e}")),
        }
    }

    /// Speaker work will not run: the final pass diarises the meeting whole, and says why.
    fn speakers_stopped(&mut self, reason: String) {
        let Some(hold) = &mut self.hold else {
            return;
        };
        hold.speakers_held = false;
        hold.speaker_cancel.cancel();
        hold.speaker_audio.clear();
        let tracks: Vec<TrackId> = self.tracks.iter().map(|t| t.kind).collect();
        self.speakers_off_tracks
            .extend(tracks.into_iter().map(|t| (t, reason.clone())));
        self.emit(RuntimeEvent::LiveSpeakersOff { reason });
    }

    fn transcription_loaded(&mut self, built: Result<Vec<BuiltTrack>, RuntimeError>) {
        let Some(hold) = &self.hold else {
            return;
        };
        if !hold.transcript_held {
            return;
        }
        match built {
            Ok(tracks) => self.attach_transcription(tracks),
            Err(e) => self.load_failed(&e.to_string()),
        }
    }

    /// Starts the track's worker and hands it everything queued, then the live summaries.
    fn attach_transcription(&mut self, tracks: Vec<BuiltTrack>) {
        let Some(hold) = &mut self.hold else {
            return;
        };
        hold.transcript_held = false;
        let mut audio = std::mem::take(&mut hold.transcript_audio);
        let was_held = self.transcript_state.is_some();
        let mut queued = 0u64;
        let mut over_cap = false;
        let mut spawned = Vec::new();
        let mut failed = None;
        for t in tracks {
            match LiveWorker::spawn(
                t.track,
                t.live,
                self.policy.pipeline.partials,
                self.worker_cap,
            ) {
                Ok(worker) => spawned.push((worker, t.backend, t.fallbacks, t.record)),
                Err(e) => failed = Some(e.to_string()),
            }
        }
        if let Some(why) = failed {
            self.load_failed(&why);
            return;
        }
        for (worker, backend, fallbacks, record) in spawned {
            let track = worker.track();
            let (chunks, frames) = audio.take(track);
            queued = queued.max(frames);
            for chunk in chunks {
                // Copied only while speaker work still holds the same chunk.
                worker.send_owned(Arc::try_unwrap(chunk).unwrap_or_else(|c| c.to_vec()));
            }
            self.sent[slot(track)] = frames;
            over_cap |= worker.backlog() > self.worker_cap;
            // After the end the request is made: it holds this track's held entry.
            if !self.pipeline.is_ended() {
                let registered = self
                    .pipeline
                    .register_held_live(worker.final_pass(), worker.backlog_source());
                if let Err(e) = registered {
                    self.warn(RuntimeWarning::Pipeline(e));
                }
            }
            if let Some(record) = record {
                self.stretch_records.push((track, record));
            }
            self.workers.push(worker);
            let stored = crate::backends::record_pass_backend(
                &mut self.services.store(),
                self.meeting_id,
                TranscriptPass::Live,
                Some(track),
                &backend,
            );
            if let Err(e) = stored {
                self.warn(RuntimeWarning::Persistence(e.to_string()));
            }
            if !fallbacks.is_empty() {
                self.warn(RuntimeWarning::BackendFallback {
                    track,
                    backend: backend.kind,
                    refused: fallbacks.clone(),
                });
            }
            self.emit(RuntimeEvent::LiveBackend {
                track,
                backend,
                fallbacks,
            });
        }
        // Whatever a track without a worker holds goes with `audio`.
        self.start_live_summaries();
        if self.workers.is_empty() {
            // No track transcribes: nothing to catch up on, and nothing to wait for.
            if was_held {
                self.set_transcript_state(LiveTranscriptState::UpToDate);
            }
        } else if was_held || queued > self.caught_up_frames() {
            self.set_transcript_state(LiveTranscriptState::CatchingUp);
        }
        // The worker stops itself when it takes its queue (`worker` module docs).
        if over_cap {
            self.set_transcript_state(LiveTranscriptState::Stopped);
        }
    }

    /// The map worker, once there is a transcript for it to read.
    fn start_live_summaries(&mut self) {
        let Some(summaries) = self.pending_summaries.take() else {
            return;
        };
        match SummaryThread::spawn(&summaries) {
            Ok(thread) => {
                self.summaries = Some(thread);
                if self.hold.as_ref().is_some_and(|h| h.watch_power) {
                    self.next_power_read.get_or_insert_with(Instant::now);
                }
            }
            Err(e) => self.emit(RuntimeEvent::LiveSummariesOff {
                reason: e.to_string(),
            }),
        }
    }

    /// The load failed: the meeting goes on recording and the final pass does the transcript.
    fn load_failed(&mut self, error: &str) {
        self.warn(RuntimeWarning::LiveLoadFailed {
            error: error.to_owned(),
        });
        self.transcript_stopped();
    }

    /// The live transcript will not run: nothing more is kept for it, and the final pass
    /// transcribes the whole meeting.
    fn transcript_stopped(&mut self) {
        if let Some(hold) = &mut self.hold {
            hold.transcript_held = false;
            hold.cancel.cancel();
            hold.transcript_audio.clear();
        }
        if self.pending_summaries.take().is_some() {
            self.emit(RuntimeEvent::LiveSummariesOff {
                reason: "the live transcript stopped".to_owned(),
            });
        }
        self.set_transcript_state(LiveTranscriptState::Stopped);
    }

    /// Ends the hold at the end of capture: work that has not started is abandoned. Waits for the
    /// hold thread, so no model load outlives the session (a final pass loads next; a load
    /// in progress cannot be interrupted). What it loaded goes with it. The wait gives way to the
    /// catch-up's cancel ([`super::LiveSession::cancel_catch_up`]) and ends after
    /// [`LiveHold::load_stall`] with nothing from the thread (T9.22b): either way the thread is
    /// left to end on its own, never joined, and only the stall is reported.
    pub(super) fn end_hold(&mut self) {
        // Nobody stopped a source that ran out by itself: its work goes on (module docs). Replays
        // only: this wait cannot be cancelled and lasts as long as the gates stay closed, which a
        // replay's own run bounds. A real capture ends `Stopped` or `Failed` and never waits here.
        if self
            .capture_end
            .is_some_and(|e| e.reason == EndReason::Finished)
        {
            while self.holding() {
                let Some(hold) = &self.hold else {
                    break;
                };
                match hold.from.recv() {
                    Ok(message) => self.take_up(message),
                    Err(_) => self.hold_thread_ended(),
                }
            }
        }
        let Some(mut hold) = self.hold.take() else {
            return;
        };
        hold.cancel.cancel();
        hold.speaker_cancel.cancel();
        // Unstarted, it has a factory that nothing will ever call.
        hold.work = None;
        if let Some(thread) = hold.thread.take() {
            match wait_for_hold_thread(&hold.from, hold.hold.load_stall, &self.catch_up) {
                HoldThreadEnd::Ended => {
                    let _ = thread.join();
                }
                // Left running: it ends by itself once its load returns, and what it loaded goes
                // nowhere (the receiver goes with `hold`).
                HoldThreadEnd::Cancelled => drop(thread),
                HoldThreadEnd::Stalled => {
                    drop(thread);
                    self.warn(RuntimeWarning::LiveLoadStalled);
                    self.problems.push(RuntimeWarning::LiveLoadStalled);
                }
            }
        }
        for note in hold.take_notes() {
            self.warn(note);
        }
        if self.pending_summaries.take().is_some() {
            self.emit(RuntimeEvent::LiveSummariesOff {
                reason: "the meeting ended before the live transcript started".to_owned(),
            });
        }
    }

    fn caught_up_frames(&self) -> u64 {
        PIPELINE_RATE.frames_for(CAUGHT_UP_BACKLOG) as u64
    }

    /// Reports a change of the transcript's state; [`LiveTranscriptState::Stopped`] is final.
    pub(super) fn set_transcript_state(&mut self, state: LiveTranscriptState) {
        if self.transcript_state == Some(state)
            || self.transcript_state == Some(LiveTranscriptState::Stopped)
        {
            return;
        }
        self.transcript_state = Some(state);
        self.emit(RuntimeEvent::LiveTranscript(state));
    }

    /// Says when a catch-up is over.
    pub(super) fn check_catch_up(&mut self) {
        if self.transcript_state == Some(LiveTranscriptState::CatchingUp)
            && !self.workers.is_empty()
            && self
                .workers
                .iter()
                .all(|w| w.backlog() <= self.caught_up_frames())
        {
            self.set_transcript_state(LiveTranscriptState::UpToDate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TODO 50: audio held for both kinds of work is one allocation, and a queue taken last
    /// gets it without a copy.
    #[test]
    fn meeting_queue_held_audio_is_kept_once_for_both_queues() {
        let (mut transcript, mut speakers) = (HeldAudio::default(), HeldAudio::default());
        let chunk = Arc::new(vec![0.25; 160]);
        let data = chunk.as_ptr();
        assert_eq!(transcript.push(TrackId::Mic, Arc::clone(&chunk)), 160);
        assert_eq!(speakers.push(TrackId::Mic, chunk), 160);
        let (held_for_speakers, frames) = speakers.take(TrackId::Mic);
        assert_eq!(frames, 160);
        assert!(std::ptr::eq(held_for_speakers[0].as_ptr(), data), "shared");
        drop(held_for_speakers);
        let (held, frames) = transcript.take(TrackId::Mic);
        assert_eq!(frames, 160);
        let owned = Arc::try_unwrap(held.into_iter().next().unwrap()).unwrap();
        assert!(std::ptr::eq(owned.as_ptr(), data), "no copy");
        assert_eq!(transcript.take(TrackId::Mic).1, 0);
    }
}
