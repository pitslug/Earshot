//! Live transcription workers (T6.10a): one thread per transcribed track, so the session
//! thread (capture, echo cancellation, FLAC) never waits on a decode.
//!
//! The session sends each `Audio` event of the track to its worker in pipeline order; the
//! worker drains everything queued before each decode, so a backlog built up during a
//! slow decode is decoded in one go and `PartialPolicy::Coalesce` skips its partials.
//! Transcript events and a `LiveAsr` error come back through a channel and are handled
//! by the session like the pipeline's own.
//!
//! Backpressure (T6.10b, the live chunker): a worker more than [`PARTIALS_OFF_BEHIND`]
//! behind decodes without partials (`PartialPolicy::Off`: finals only, every word still
//! reaches a final) until it is back within [`PARTIALS_ON_BEHIND`], and reports each
//! switch, including one made while finishing. Speech stretches (T6.13b) decode finals
//! only and never switch. [`LiveWorker::finish`] decodes the rest, closes every open
//! segment and joins the thread.
//!
//! A stretch worker working through a backlog (a held session's queue, T9.1l) takes it
//! [`CATCH_UP_SLICE`] at a time, so what it decodes reaches the transcript as it goes rather than
//! all at the end; it decodes the same pieces either way. A queue past `cap` is taken whole, which
//! makes the track stop as "fell behind" (`LiveTrack::feed`).
//!
//! Speaker work never runs here: it has its own thread (`super::speaker_thread`, T6.13d3),
//! so a decode never waits for it.
//!
//! A stretch worker decodes its track's audio quantised to 16 bits exactly as the FLAC
//! stores it (`earshot_core::audio::quantise_pcm16`, T6.13e, as the speaker thread does),
//! so its pieces are the ones the whole-meeting pass decodes from the recording.
//!
//! A stretch worker also keeps what it decoded ([`LiveWorker::stretch_log`]: each piece as
//! decoded and the frontier after it, T6.13c), updated under one lock after every engine
//! call (`LiveTrack::run` or `finish`), also one that ended in a panic, so the session can
//! record it even when the worker failed or crashed; the transcript events of such a call
//! are sent before the panic continues. Core keeps each piece (and queues its final) as
//! soon as its own decode returns (`PieceStream::push_each`), so only the piece whose
//! decode panicked is lost: earlier pieces of the same core call are logged and shown.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use earshot_core::asr::{AsrError, CancelToken, DecodedPieces};
use earshot_core::audio::{PIPELINE_RATE, Sample, quantise_pcm16};
use earshot_core::capture::TrackId;
use earshot_core::pipeline::{
    FinalPassTrack, LiveTrack, PartialPolicy, PipelineError, PipelineEvent,
};

use crate::RuntimeError;

/// What a stretch worker decoded ([`LiveWorker::stretch_log`]): the pieces with their
/// frontier, and the backend its transcriber reported after the last engine call
/// (`LiveTrack::stretches_backend`), which is what the final pass reports when it reuses
/// every piece.
#[derive(Debug, Clone, Default)]
pub(super) struct StretchLog {
    pub(super) decoded: DecodedPieces,
    pub(super) backend: Option<earshot_core::asr::BackendInfo>,
}

enum ToWorker {
    Audio(Vec<Sample>),
    Finish,
}

/// The most audio a stretch worker takes into one engine call while its queue is within the cap.
pub const CATCH_UP_SLICE: Duration = Duration::from_secs(30);

/// A worker more than this far behind switches partial decodes off.
pub const PARTIALS_OFF_BEHIND: Duration = Duration::from_secs(6);
/// A worker that switched partials off switches them back on within this.
pub const PARTIALS_ON_BEHIND: Duration = Duration::from_secs(2);

/// A partial-decode switch: whether partials are on, and how far behind the track was.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PartialsSwitch {
    pub enabled: bool,
    pub behind: Duration,
}

enum FromWorker {
    Events(Vec<PipelineEvent>),
    Partials(PartialsSwitch),
    Done,
}

/// The policy after a drain that left `behind` undecoded (hysteresis, module docs).
fn partials_on(was_on: bool, behind: Duration) -> bool {
    if was_on {
        behind <= PARTIALS_OFF_BEHIND
    } else {
        behind < PARTIALS_ON_BEHIND
    }
}

/// A track's live ASR on its own thread.
pub(super) struct LiveWorker {
    track: TrackId,
    to: Sender<ToWorker>,
    from: Receiver<FromWorker>,
    /// Samples sent and not yet decoded (sent minus fed into the chunker and run).
    queued: Arc<AtomicU64>,
    /// Pipeline frames up to which the track is transcribed (`LiveTrack::decoded_through`
    /// after each decode).
    decoded: Arc<AtomicU64>,
    /// Whether the engine decodes partials (the chunker) or finals only (stretches).
    partials: bool,
    cancel: Option<CancelToken>,
    final_pass: FinalPassTrack,
    /// What the stretches decoded so far; `None` for the chunker.
    stretches: Option<Arc<Mutex<StretchLog>>>,
    thread: Option<JoinHandle<()>>,
}

impl LiveWorker {
    /// Starts the worker for `live`; `cap` is the pipeline's `max_asr_backlog` in samples.
    pub(super) fn spawn(
        track: TrackId,
        mut live: LiveTrack,
        policy: PartialPolicy,
        cap: u64,
    ) -> Result<Self, RuntimeError> {
        let (to, inbox) = channel::<ToWorker>();
        let (outbox, from) = channel::<FromWorker>();
        let queued = Arc::new(AtomicU64::new(0));
        let decoded = Arc::new(AtomicU64::new(0));
        let cancel = live.cancel_token();
        let final_pass = live.final_pass();
        let partials = live.has_partials();
        let counter = Arc::clone(&queued);
        let progress = Arc::clone(&decoded);
        let stretches = live
            .stretches_through()
            .map(|_| Arc::new(Mutex::new(StretchLog::default())));
        let log = stretches.clone();
        // A stretch worker decodes the samples the FLAC stores (T6.13e), so the final pass
        // that reuses its pieces equals the whole-meeting pass on the recording.
        let quantise = stretches.is_some();
        let thread = std::thread::Builder::new()
            .name(format!("earshot-live-{track:?}").to_lowercase())
            .spawn(move || {
                let mut events = Vec::new();
                let mut finish = false;
                let mut on = true;
                let slice = PIPELINE_RATE.frames_for(CATCH_UP_SLICE) as u64;
                while let Ok(first) = inbox.recv() {
                    // Everything queued so far is decoded in one go, or, for the stretches, at
                    // most a slice of it unless the queue is past the cap (module docs).
                    let whole = !quantise || counter.load(Ordering::SeqCst) > cap;
                    let mut n = 0u64;
                    let mut next = Some(first);
                    while let Some(msg) = next.take() {
                        match msg {
                            ToWorker::Audio(samples) => {
                                n += samples.len() as u64;
                                if quantise {
                                    live.feed(&quantise_pcm16(&samples), cap, &mut events);
                                } else {
                                    live.feed(&samples, cap, &mut events);
                                }
                            }
                            ToWorker::Finish => finish = true,
                        }
                        if whole || n < slice {
                            next = inbox.try_recv().ok();
                        }
                    }
                    let behind = PIPELINE_RATE
                        .duration_of(usize::try_from(live.backlog()).unwrap_or(usize::MAX));
                    // Speech stretches have no partials to switch off.
                    let now_on = !partials || partials_on(on, behind);
                    if now_on != on {
                        on = now_on;
                        let _ = outbox.send(FromWorker::Partials(PartialsSwitch {
                            enabled: on,
                            behind,
                        }));
                    }
                    let effective = if on { policy } else { PartialPolicy::Off };
                    // A panicking transcriber must not lose what earlier core calls of this
                    // engine call decoded (`finish` runs the backlog, then the last pieces):
                    // it is logged and its events are sent before the panic continues.
                    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if finish {
                            live.finish(effective, &mut events);
                        } else {
                            live.run(effective, &mut events);
                        }
                    }));
                    if let (Some(log), Some(through)) = (&log, live.stretches_through()) {
                        let pieces = live.take_decoded_pieces();
                        let mut log = log
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        log.decoded.pieces.extend(pieces);
                        log.decoded.through = usize::try_from(through).unwrap_or(usize::MAX);
                        log.backend = live.stretches_backend().cloned();
                    }
                    if let Err(panic) = ran {
                        if !events.is_empty() {
                            let _ = outbox.send(FromWorker::Events(std::mem::take(&mut events)));
                        }
                        std::panic::resume_unwind(panic);
                    }
                    // Events first, then the progress they cover: a session that reads the
                    // progress before draining the events (the live echo hold) then always
                    // finds them queued.
                    if !events.is_empty() {
                        let _ = outbox.send(FromWorker::Events(std::mem::take(&mut events)));
                    }
                    progress.store(live.decoded_through(), Ordering::SeqCst);
                    counter.fetch_sub(n, Ordering::SeqCst);
                    if finish {
                        break;
                    }
                }
                let _ = outbox.send(FromWorker::Done);
            })
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            track,
            to,
            from,
            queued,
            decoded,
            partials,
            cancel,
            final_pass,
            stretches,
            thread: Some(thread),
        })
    }

    /// The track.
    pub(super) fn track(&self) -> TrackId {
        self.track
    }

    /// Queues the track's next pipeline audio.
    pub(super) fn send(&self, samples: &[Sample]) {
        self.queued
            .fetch_add(samples.len() as u64, Ordering::SeqCst);
        if self.to.send(ToWorker::Audio(samples.to_vec())).is_err() {
            // The worker is gone (it can only end after Finish); nothing will decode this.
            self.queued
                .fetch_sub(samples.len() as u64, Ordering::SeqCst);
        }
    }

    /// [`LiveWorker::send`] for audio the session no longer needs.
    pub(super) fn send_owned(&self, samples: Vec<Sample>) {
        let n = samples.len() as u64;
        self.queued.fetch_add(n, Ordering::SeqCst);
        if self.to.send(ToWorker::Audio(samples)).is_err() {
            self.queued.fetch_sub(n, Ordering::SeqCst);
        }
    }

    /// Samples sent and not yet decoded.
    pub(super) fn backlog(&self) -> u64 {
        self.queued.load(Ordering::SeqCst)
    }

    /// [`LiveWorker::backlog`] for the pipeline's stats (`Pipeline::register_external_live`).
    pub(super) fn backlog_source(&self) -> Box<dyn Fn() -> u64 + Send> {
        let queued = Arc::clone(&self.queued);
        Box::new(move || queued.load(Ordering::SeqCst))
    }

    /// Meeting time the track has been transcribed up to (`LiveTrack::decoded_through`:
    /// fed into the chunker and run, or the stretches' decoded frontier). The worker
    /// publishes it after the events of that decode, so every event up to it is queued
    /// for the next [`LiveWorker::drain`] by the time it is read here.
    pub(super) fn decoded_through(&self) -> earshot_core::asr::MeetingTime {
        earshot_core::asr::MeetingTime::from_pipeline_frames(self.decoded.load(Ordering::SeqCst))
    }

    /// The counter behind [`LiveWorker::decoded_through`] (pipeline frames), read by the track's
    /// previews (`preview` module docs). Only the worker writes it.
    pub(super) fn frontier_source(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.decoded)
    }

    /// The counter behind [`LiveWorker::backlog`] (samples), read by the track's previews.
    pub(super) fn queue_source(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.queued)
    }

    /// Whether the track decodes partials (the chunker) or finals only (speech stretches).
    pub(super) fn has_partials(&self) -> bool {
        self.partials
    }

    /// The final-pass entry of the track (language and `Track` row of the live ASR).
    pub(super) fn final_pass(&self) -> FinalPassTrack {
        self.final_pass.clone()
    }

    /// What the track's speech stretches decoded so far (shared with the worker thread,
    /// still readable after it ended or crashed); `None` for the live chunker.
    pub(super) fn stretch_log(&self) -> Option<Arc<Mutex<StretchLog>>> {
        self.stretches.clone()
    }

    /// Cancels the decode in progress (the track's live ASR then stops with an error).
    pub(super) fn cancel(&self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
    }

    /// Events and partial-decode switches produced since the last call, without waiting.
    pub(super) fn drain(&self) -> (Vec<PipelineEvent>, Vec<PartialsSwitch>) {
        let mut out = Vec::new();
        let mut switches = Vec::new();
        for msg in self.from.try_iter() {
            match msg {
                FromWorker::Events(e) => out.extend(e),
                FromWorker::Partials(p) => switches.push(p),
                FromWorker::Done => {}
            }
        }
        (out, switches)
    }

    /// Decodes the rest and closes every open segment, then joins the thread; returns the
    /// remaining events and partial switches (a worker behind at the end decodes the rest
    /// with partials off, as during the meeting).
    pub(super) fn finish(mut self) -> (Vec<PipelineEvent>, Vec<PartialsSwitch>) {
        let _ = self.to.send(ToWorker::Finish);
        let mut out = Vec::new();
        let mut switches = Vec::new();
        for msg in self.from.iter() {
            match msg {
                FromWorker::Events(e) => out.extend(e),
                FromWorker::Partials(p) => switches.push(p),
                FromWorker::Done => break,
            }
        }
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            // A panicking transcriber ended the thread: the track's live transcript stops
            // there, and the user is told (the recording is unaffected).
            out.push(PipelineEvent::Error(PipelineError::LiveAsr {
                track: self.track,
                error: AsrError::Backend("the live transcription thread crashed".into()),
            }));
        }
        (out, switches)
    }
}

impl Drop for LiveWorker {
    fn drop(&mut self) {
        // Not finished (the session failed): stop the decode and let the thread end.
        if let Some(t) = self.thread.take() {
            self.cancel();
            let _ = self.to.send(ToWorker::Finish);
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_worker_partials_switch_with_hysteresis() {
        let s = Duration::from_secs;
        assert!(partials_on(true, s(6)), "at the limit: still on");
        assert!(!partials_on(true, s(7)), "beyond it: off");
        assert!(!partials_on(false, s(3)), "off stays off until within 2 s");
        assert!(!partials_on(false, s(2)));
        assert!(partials_on(false, Duration::from_millis(1_999)));
    }
}
