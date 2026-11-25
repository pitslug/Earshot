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
//! A worker that does not take its queue (a decode that never returns) is sent no more audio past
//! its cap (T14.21), so a hung decode costs a bounded queue, not the rest of the meeting's audio
//! in memory. The session stops sending once more than [`cut_at`] samples wait in the channel:
//! the cap, one slice and two of the largest messages sent. Whatever the worker is doing then, an
//! alive one stops the track as "fell behind" the next time it takes its queue, exactly as it
//! would have had the audio kept coming (a queue past `cap` at that moment is taken whole and
//! stops the track in `LiveTrack::feed`):
//! - in a decode, or about to take its queue: more than `cap` waits and nothing is taken from it
//!   before that take;
//! - taking its queue whole: it takes everything, more than `cap`;
//! - taking a slice: it takes at most a slice and one message more, and the count of what it
//!   took (`taken`) trails by at most the message in its hands, so more than `cap` still waits
//!   afterwards and the next take is whole.
//!
//! So no worker the rule before would have let go on is cut. Audio the session does not send is
//! counted ([`ToWorker::Unsent`], at most one such message waiting), and an alive worker adds
//! it to what its stopped track received, as it did when the audio itself came: its frontier
//! follows the meeting as before (the echo hold, the previews and the live summaries read it).
//! A worker whose thread is stuck is left when it is dropped, never joined ([`DROP_WAIT`]).
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

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    /// Audio was not sent (the queue is past its cap, module docs): the count is in
    /// [`Unsent::samples`].
    Unsent,
    Finish,
}

/// Audio the session did not send to a worker past its cap: the samples the worker has not
/// counted yet, and whether a [`ToWorker::Unsent`] is on its way (so at most one waits in the
/// channel however long the worker takes).
#[derive(Default)]
struct Unsent {
    samples: AtomicU64,
    told: AtomicBool,
}

/// How long a dropped worker's thread is waited for after its decode was cancelled: a judgement
/// (a healthy decode sees the cancel well within it, and a failing session can afford it), not a
/// measurement. A thread still running then is stuck in a call that never returns and is left,
/// never joined (T14.21).
pub const DROP_WAIT: Duration = Duration::from_secs(2);

/// The waiting samples past which a worker with `cap` is sent no more audio (module docs), given
/// the largest message sent to it so far.
fn cut_at(cap: u64, largest: u64) -> u64 {
    let slice = PIPELINE_RATE.frames_for(CATCH_UP_SLICE) as u64;
    cap.saturating_add(slice)
        .saturating_add(largest.saturating_mul(2))
}

/// Whether `events` hold the error a track's live ASR stops with (`LiveTrack` sends one only
/// when it stops).
fn stopped_in(events: &[PipelineEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, PipelineEvent::Error(PipelineError::LiveAsr { .. })))
}

/// Adds `n` samples the session did not send to what a stopped `live` received (a stopped
/// track only counts what it is fed).
fn count_unsent(live: &mut LiveTrack, mut n: u64, cap: u64, events: &mut Vec<PipelineEvent>) {
    const SILENCE: [Sample; 1600] = [0.0; 1600];
    while n > 0 {
        let k = usize::try_from(n).map_or(SILENCE.len(), |n| n.min(SILENCE.len()));
        live.feed(&SILENCE[..k], cap, events);
        n -= k as u64;
    }
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

/// How a worker's catch-up at the end of a meeting ended ([`LiveWorker::finish_watched`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatchUpEnd {
    /// It decoded its queue and its thread ended.
    Finished,
    /// The meeting's catch-up was cancelled: the decode was asked to stop and the thread left.
    Cancelled,
    /// Nothing moved for the stall bound: the thread was left (never joined).
    Stalled,
}

/// The no-progress rule of a catch-up (T9.22): it stalls only when none of its signs of life
/// (`sign`: the decoded frontier, the decode's cancel-token polls, messages from the worker)
/// changed for `bound`. However long a decode takes, it never stalls while it moves, provided no single
/// call between two polls of its token takes `bound` (Parakeet polls once per piece and chunk,
/// Whisper inside its graph compute: `LiveConfig::catch_up_stall`). Only the worker thread may poll a
/// worker's token: a second poller would keep a hung decode looking alive (pinned by
/// `live_worker_token_is_polled_only_by_the_worker`).
#[derive(Debug, Clone)]
pub(super) struct StallWatch {
    bound: Duration,
    sign: Option<[u64; 3]>,
    since: Duration,
}

impl StallWatch {
    pub(super) fn new(bound: Duration) -> Self {
        Self {
            bound,
            sign: None,
            since: Duration::ZERO,
        }
    }

    /// Looks at `sign` at `now` (time since the watch began): whether it has stalled.
    pub(super) fn stalled(&mut self, sign: [u64; 3], now: Duration) -> bool {
        if self.sign != Some(sign) {
            self.sign = Some(sign);
            self.since = now;
            return false;
        }
        now.saturating_sub(self.since) >= self.bound
    }
}

/// How often a watched catch-up looks at its worker.
const WATCH_POLL: Duration = Duration::from_millis(50);

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
    /// The pipeline's `max_asr_backlog` in samples.
    cap: u64,
    /// Samples sent, and the largest message among them.
    sent: Cell<u64>,
    largest: Cell<u64>,
    /// Samples the thread has taken off its channel (counted right after each message).
    taken: Arc<AtomicU64>,
    /// Past its cap: no more audio is sent, only counted (module docs).
    cut: Cell<bool>,
    unsent: Arc<Unsent>,
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
        let taken = Arc::new(AtomicU64::new(0));
        let took = Arc::clone(&taken);
        let unsent = Arc::new(Unsent::default());
        let not_sent = Arc::clone(&unsent);
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
                // Whether the track's live ASR has stopped, and unsent samples met before it had.
                let mut stopped = false;
                let mut owed = 0u64;
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
                                took.fetch_add(samples.len() as u64, Ordering::SeqCst);
                                n += samples.len() as u64;
                                let before = events.len();
                                if quantise {
                                    live.feed(&quantise_pcm16(&samples), cap, &mut events);
                                } else {
                                    live.feed(&samples, cap, &mut events);
                                }
                                stopped = stopped || stopped_in(&events[before..]);
                            }
                            ToWorker::Unsent => {
                                // Cleared before the count is read, so samples added after the
                                // read are told again.
                                not_sent.told.store(false, Ordering::SeqCst);
                                owed += not_sent.samples.swap(0, Ordering::SeqCst);
                                // The audio before this message stopped the track when it was
                                // taken (module docs). Were the track still live, the count
                                // would wait (`owed`): a live track is never fed silence in
                                // place of audio it was not sent, and a stream that simply
                                // ends early is one the final pass finishes from the recording.
                            }
                            ToWorker::Finish => finish = true,
                        }
                        if stopped && owed > 0 {
                            count_unsent(&mut live, std::mem::take(&mut owed), cap, &mut events);
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
                    stopped = stopped || stopped_in(&events);
                    if stopped && owed > 0 {
                        count_unsent(&mut live, std::mem::take(&mut owed), cap, &mut events);
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
            cap,
            sent: Cell::new(0),
            largest: Cell::new(0),
            taken,
            cut: Cell::new(false),
            unsent,
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

    /// Queues the track's next pipeline audio; past the cap it is only counted (module docs).
    pub(super) fn send(&self, samples: &[Sample]) {
        let n = samples.len() as u64;
        if !self.admit(n) {
            return;
        }
        self.queued.fetch_add(n, Ordering::SeqCst);
        if self.to.send(ToWorker::Audio(samples.to_vec())).is_err() {
            // The worker is gone (it can only end after Finish); nothing will decode this.
            self.queued.fetch_sub(n, Ordering::SeqCst);
        } else {
            self.sent_now(n);
        }
    }

    /// [`LiveWorker::send`] for audio the session no longer needs.
    pub(super) fn send_owned(&self, samples: Vec<Sample>) {
        let n = samples.len() as u64;
        if !self.admit(n) {
            return;
        }
        self.queued.fetch_add(n, Ordering::SeqCst);
        if self.to.send(ToWorker::Audio(samples)).is_err() {
            self.queued.fetch_sub(n, Ordering::SeqCst);
        } else {
            self.sent_now(n);
        }
    }

    /// Whether the next `n` samples go to the worker. Past its cap they are counted instead, and
    /// the worker is told once until it has read the count.
    fn admit(&self, n: u64) -> bool {
        if !self.cut.get() {
            return true;
        }
        self.unsent.samples.fetch_add(n, Ordering::SeqCst);
        if !self.unsent.told.swap(true, Ordering::SeqCst) {
            let _ = self.to.send(ToWorker::Unsent);
        }
        false
    }

    /// Counts `n` samples as sent, and decides whether the worker is past its cap.
    fn sent_now(&self, n: u64) {
        self.sent.set(self.sent.get().saturating_add(n));
        self.largest.set(self.largest.get().max(n));
        if self.waiting() > cut_at(self.cap, self.largest.get()) {
            self.cut.set(true);
        }
    }

    /// Samples sent that the thread has not taken off its channel (never less than what waits
    /// there: the thread counts a message right after taking it).
    pub(super) fn waiting(&self) -> u64 {
        self.sent
            .get()
            .saturating_sub(self.taken.load(Ordering::SeqCst))
    }

    /// Whether the worker went past its cap and is sent no more audio.
    #[cfg(test)]
    pub(super) fn past_cap(&self) -> bool {
        self.cut.get()
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

    /// Whether the worker's thread has ended. Before [`LiveWorker::finish`] that means it
    /// crashed: its frontier stays where it was for the rest of the meeting.
    pub(super) fn ended(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
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

    /// [`LiveWorker::finish`] that never waits for ever (T9.22): `cancel` (the meeting's catch-up
    /// token) or a stall ([`StallWatch`] over `stall`) asks the decode to stop and leaves the
    /// thread to end on its own, never joined, as a stalled GPU summary is left (T8.4ab). Returns
    /// the events received until then.
    pub(super) fn finish_watched(
        mut self,
        stall: Duration,
        cancel: &CancelToken,
    ) -> (Vec<PipelineEvent>, Vec<PartialsSwitch>, CatchUpEnd) {
        let _ = self.to.send(ToWorker::Finish);
        let mut out = Vec::new();
        let mut switches = Vec::new();
        let mut watch = StallWatch::new(stall);
        let started = Instant::now();
        let mut messages = 0u64;
        let polls = self.cancel.clone();
        let end = loop {
            match self.from.recv_timeout(WATCH_POLL) {
                Ok(FromWorker::Events(e)) => {
                    messages += 1;
                    out.extend(e);
                }
                Ok(FromWorker::Partials(p)) => {
                    messages += 1;
                    switches.push(p);
                }
                Ok(FromWorker::Done) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break CatchUpEnd::Finished;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancel.is_cancelled() {
                break CatchUpEnd::Cancelled;
            }
            let sign = [
                self.decoded.load(Ordering::SeqCst),
                polls.as_ref().map_or(0, CancelToken::polls),
                messages,
            ];
            if watch.stalled(sign, started.elapsed()) {
                break CatchUpEnd::Stalled;
            }
        };
        if end == CatchUpEnd::Finished {
            let (rest, more) = self.finish();
            out.extend(rest);
            switches.extend(more);
        } else {
            // Left running: it ends by itself once its call returns (it sees the cancel).
            self.cancel();
            drop(self.thread.take());
        }
        (out, switches, end)
    }
}

impl LiveWorker {
    /// Stops the decode and lets the thread end, waiting at most `wait` for it: a thread stuck
    /// in a call that never returns is left to end by itself, never joined (T14.21), as a
    /// stalled catch-up is ([`LiveWorker::finish_watched`]). Returns whether the thread has
    /// ended (also when it had been finished or left before).
    pub(super) fn end_within(&mut self, wait: Duration) -> bool {
        let Some(t) = self.thread.take() else {
            return true;
        };
        self.cancel();
        let _ = self.to.send(ToWorker::Finish);
        let limit = Instant::now() + wait;
        while !t.is_finished() && Instant::now() < limit {
            std::thread::sleep(Duration::from_millis(1));
        }
        let ended = t.is_finished();
        if ended {
            // It has ended: this returns at once.
            let _ = t.join();
        }
        ended
    }
}

impl Drop for LiveWorker {
    fn drop(&mut self) {
        // Not finished (the session failed): the thread is waited for only as long as a healthy
        // decode takes to see the cancel.
        self.end_within(DROP_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_catch_up_stalls_only_when_nothing_moves_for_the_bound() {
        // T9.22, clock-free: times are given, never read.
        let s = Duration::from_secs;
        let mut w = StallWatch::new(s(120));
        assert!(!w.stalled([0, 0, 0], s(0)));
        assert!(!w.stalled([0, 0, 0], s(119)));
        assert!(w.stalled([0, 0, 0], s(120)), "no movement for the bound");
        // Any sign of life starts the bound again: the frontier, the decode's polls, a message.
        for (k, sign) in [[1, 0, 0], [1, 1, 0], [1, 1, 1]].into_iter().enumerate() {
            let at = s(200 + 100 * k as u64);
            assert!(!w.stalled(sign, at));
            assert!(!w.stalled(sign, at + s(119)));
        }
        // A decode that polls slowly but steadily is never cut, however long it takes.
        let mut slow = StallWatch::new(s(120));
        for minute in 0..600u64 {
            assert!(!slow.stalled([0, minute, 0], s(60 * minute)));
        }
    }

    #[test]
    fn live_worker_token_is_polled_only_by_the_worker() {
        // T9.22: the decode's cancel-token polls are a sign of life only if nothing else polls the
        // token. The worker is the one place in this crate that takes a live track's token.
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    rs_files(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src, &mut files);
        let needle = concat!(".cancel", "_token()");
        let takers: Vec<String> = files
            .iter()
            .filter(|p| std::fs::read_to_string(p).unwrap().contains(needle))
            .map(|p| {
                p.strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(takers, ["live/worker.rs"]);
        // And within it, once: the token handed to the decode, kept for cancel and polls().
        let me = std::fs::read_to_string(src.join("live").join("worker.rs")).unwrap();
        assert_eq!(me.matches(needle).count(), 1);
    }

    #[test]
    fn live_worker_cut_point_leaves_more_than_the_cap_waiting() {
        // T14.21, clock-free: when the session stops sending, a worker in the middle of taking a
        // slice may still take the rest of it and one message more, and the count of what it took
        // may trail by the message in its hands. More than the cap waits even then, so its next
        // take is whole and stops the track.
        let slice = PIPELINE_RATE.frames_for(CATCH_UP_SLICE) as u64;
        for cap in [0, 16_000, 480_000, 30 * 60 * 16_000] {
            for largest in [0, 160, 1_600, 480_000, 30 * 60 * 16_000] {
                let waiting = cut_at(cap, largest) + 1;
                let uncounted = largest;
                let rest_of_slice = slice + largest;
                assert!(
                    waiting - uncounted - rest_of_slice > cap,
                    "cap {cap}, largest {largest}"
                );
            }
        }
        // No cap, no cut.
        assert_eq!(cut_at(u64::MAX, 1_600), u64::MAX);
        assert_eq!(cut_at(1_600, u64::MAX), u64::MAX);
    }

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
