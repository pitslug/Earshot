//! Live speaker work on its own thread (T6.13d2, moved off the transcription workers in
//! T6.13d3).
//!
//! One speaker thread per session serves every track with speaker models, one batch at a
//! time, round robin. The session sends it each track's pipeline audio (as it sends the
//! transcription workers), quantised by the thread to 16 bits exactly as the FLAC stores
//! it, into the track's [`DiarStream`]; the thread computes whole batches of speaker
//! windows ([`earshot_core::diar::Diarizer::advance`]) as soon as their audio is known, and
//! after each finished batch copies the results to the track's snapshot, which is what the
//! session records.
//!
//! Transcription never waits for it: the transcription workers share nothing with this
//! thread, and the thread (and the ONNX Runtime pool threads of its models,
//! [`crate::backends::with_live_speakers`]) runs at below-normal OS priority
//! ([`crate::priority`]), so it only uses CPU time that transcription, capture and other
//! applications leave. A thread that cannot lower its priority stops speaker work at once.
//! The audio a track holds for speaker work (sent and not yet in the stream, plus buffered
//! in it) is bounded: past [`SPEAKER_MAX_BUFFER`] the session sends no more and speaker
//! work stops for the meeting with that reason, as it does on a model error or a crash of
//! the thread; the windows computed before stay valid and the final pass computes the rest.
//!
//! The end: [`SpeakerThread::request_stop`] (called before the transcription workers are
//! finished) trips the batch in progress, which the diarizer rolls back, and no batch
//! starts after it; the final pass does the tail. [`SpeakerThread::finish`] then waits at
//! most `wait` ([`super::LiveConfig::speaker_stop_wait`]) for the thread to end, so speaker
//! work never makes the end of a meeting wait longer than that. A thread still inside a
//! model call then (one that ignores the cancel token, or one the OS has not scheduled) is
//! left to end on its own; the snapshot holds every batch finished before, exactly what an
//! interrupted batch leaves, so the outcome is the same as a prompt stop.
//!
//! A thread left like that writes nothing after `finish` returns (so nothing after the
//! session's `Ended`): no snapshot, no [`super::LiveSpeakers::progress`]. Until its model
//! call returns it still holds its models (the ONNX sessions) and the audio it was given
//! (at most [`SPEAKER_MAX_BUFFER`] per track); it then sees the stop and exits, dropping
//! them. If the process exits first, the thread ends with it (std does not join detached
//! threads); nothing it holds is persistent.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::audio::{PIPELINE_RATE, Sample, quantise_pcm16};
use earshot_core::capture::TrackId;
use earshot_core::diar::{DiarError, DiarStream, WindowResults};

use super::LiveDiarizer;
use crate::RuntimeError;

/// Speaker work stops for the rest of the meeting when a track holds more audio than this
/// for it (module docs). Computing a batch of windows needs about 73 s at the default
/// settings (32 windows of 10 s every 1 s, plus a batch whose regions wait to be embedded),
/// so this leaves more than 100 s of slack before a slow or starved thread gives up.
pub const SPEAKER_MAX_BUFFER: Duration = Duration::from_secs(180);

fn max_held() -> u64 {
    PIPELINE_RATE.frames_for(SPEAKER_MAX_BUFFER) as u64
}

fn behind_reason() -> String {
    format!(
        "live speaker work fell more than {} s behind",
        SPEAKER_MAX_BUFFER.as_secs()
    )
}

/// A track's speaker windows, owned by the thread (module docs).
enum SpeakerWork {
    /// Computing windows as audio arrives.
    Running(Box<DiarStream>),
    /// Stopped early.
    Stopped,
}

impl SpeakerWork {
    /// Appends `samples`, quantised as the FLAC stores them.
    fn append(&mut self, samples: &[Sample]) {
        if let SpeakerWork::Running(stream) = self {
            stream.append(&quantise_pcm16(samples));
        }
    }

    fn ready(&self) -> bool {
        matches!(self, SpeakerWork::Running(s) if s.ready_batches() > 0)
    }
}

/// A track's state shared by the thread and the session.
struct Shared {
    held: Held,
    /// The windows of every finished batch, and why speaker work stopped early, if it did.
    done: Mutex<(WindowResults, Option<String>)>,
}

impl Shared {
    fn stop(&self, why: String) {
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        if done.1.is_none() {
            done.1 = Some(why);
        }
    }
}

/// Audio a track holds for speaker work, in samples: sent by the session and not yet
/// appended (`pending`) plus held by the stream (`buffered`, published by the thread).
#[derive(Debug, Default)]
struct Held {
    pending: AtomicU64,
    buffered: AtomicU64,
    /// Set once the track would have held more than [`SPEAKER_MAX_BUFFER`]; never cleared.
    overflow: AtomicBool,
    /// Set once speaker work stopped for the track (a model error, a refused priority):
    /// its audio is no longer sent. Never cleared.
    stopped: AtomicBool,
}

impl Held {
    /// Counts `n` more samples, unless the track's speaker work stopped or the track would
    /// then hold more than `cap` (it is then over the bound for good): nothing is counted
    /// and the caller sends nothing.
    fn admit(&self, n: u64, cap: u64) -> bool {
        if self.overflow.load(Ordering::SeqCst) || self.stopped.load(Ordering::SeqCst) {
            return false;
        }
        let held = self.pending.load(Ordering::SeqCst) + self.buffered.load(Ordering::SeqCst);
        if held + n > cap {
            self.overflow.store(true, Ordering::SeqCst);
            return false;
        }
        self.pending.fetch_add(n, Ordering::SeqCst);
        true
    }
}

/// A track's speaker models, for [`SpeakerThread::spawn`].
pub(crate) struct SpeakerTrack {
    pub track: TrackId,
    pub diarizer: LiveDiarizer,
    /// ONNX Runtime threads of the models (recorded with the windows).
    pub threads: Option<usize>,
    /// Windows computed so far, updated after each finished batch.
    pub progress: Option<Arc<AtomicUsize>>,
    /// Run below normal priority ([`super::LiveSpeakers::background`]).
    pub background: bool,
}

/// What the thread keeps of a track.
struct Engine {
    diarizer: LiveDiarizer,
    work: SpeakerWork,
    shared: Arc<Shared>,
    progress: Option<Arc<AtomicUsize>>,
}

impl Engine {
    fn stop(&mut self, why: String) {
        self.work = SpeakerWork::Stopped;
        self.shared.held.stopped.store(true, Ordering::SeqCst);
        self.shared.stop(why);
    }
}

/// Publishes a track's finished batch (snapshot and progress), unless the session stopped
/// waiting for this thread (`detached`): a detached thread never writes again. The flag is
/// read under the snapshot lock, which [`SpeakerThread::finish`] takes after setting it, so
/// every publish either completes before `finish` returns or does not happen.
fn publish(
    shared: &Shared,
    progress: Option<&AtomicUsize>,
    stream: &DiarStream,
    detached: &AtomicBool,
) {
    let mut done = shared.done.lock().unwrap_or_else(PoisonError::into_inner);
    if detached.load(Ordering::SeqCst) {
        return;
    }
    done.0.clone_from(stream.results());
    if let Some(p) = progress {
        p.store(stream.windows_done(), Ordering::SeqCst);
    }
}

/// A track's windows at the end and why speaker work stopped early, if it did; `None` if
/// they could not be read.
pub(crate) type SpeakerOutcome = Option<(WindowResults, Option<String>)>;

/// The session's speaker thread (module docs).
pub(crate) struct SpeakerThread {
    to: Option<Sender<(usize, Vec<Sample>)>>,
    sides: Vec<(TrackId, Arc<Shared>)>,
    cancel: CancelToken,
    /// Set by [`SpeakerThread::finish`] when it stops waiting ([`publish`]).
    detached: Arc<AtomicBool>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl SpeakerThread {
    /// Starts the thread for `tracks`.
    pub(crate) fn spawn(tracks: Vec<SpeakerTrack>) -> Result<Self, RuntimeError> {
        let mut sides = Vec::new();
        let mut engines = Vec::new();
        let background = tracks.iter().any(|t| t.background);
        for t in tracks {
            let stream = t.diarizer.stream();
            let shared = Arc::new(Shared {
                held: Held::default(),
                done: Mutex::new((stream.results().clone(), None)),
            });
            sides.push((t.track, Arc::clone(&shared)));
            engines.push(Engine {
                diarizer: t.diarizer,
                work: SpeakerWork::Running(Box::new(stream)),
                shared,
                progress: t.progress,
            });
        }
        let (to, inbox) = channel();
        let (done_tx, done) = channel();
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let detached = Arc::new(AtomicBool::new(false));
        let left = Arc::clone(&detached);
        let thread = std::thread::Builder::new()
            .name("earshot-live-speakers".into())
            .spawn(move || {
                run(&mut engines, &inbox, &token, &left, background);
                let _ = done_tx.send(());
            })
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            to: Some(to),
            sides,
            cancel,
            detached,
            done,
            thread: Some(thread),
        })
    }

    /// Queues `track`'s next pipeline audio (never blocks); a track without speaker models
    /// is ignored, and a track over [`SPEAKER_MAX_BUFFER`] gets nothing more.
    pub(crate) fn send(&self, track: TrackId, samples: &[Sample]) {
        let Some(i) = self.sides.iter().position(|(t, _)| *t == track) else {
            return;
        };
        let held = &self.sides[i].1.held;
        let n = samples.len() as u64;
        if !held.admit(n, max_held()) {
            return;
        }
        let sent = self
            .to
            .as_ref()
            .is_some_and(|to| to.send((i, samples.to_vec())).is_ok());
        if !sent {
            // The thread is gone (it crashed); nothing will take these.
            held.pending.fetch_sub(n, Ordering::SeqCst);
        }
    }

    /// Stops speaker work: the batch in progress is interrupted and none starts after it.
    pub(crate) fn request_stop(&self) {
        self.cancel.cancel();
    }

    /// Stops speaker work for the rest of the meeting because of `why` (the speaker
    /// detection setting, T6.13d4): as [`SpeakerThread::request_stop`], and every track
    /// still running gets no more audio and records `why`. The batches finished before stay
    /// valid. Returns whether any track was still running (a track already stopped keeps
    /// its own reason, and a thread that already ended, which during the meeting means it
    /// crashed, keeps the crash as the reason [`SpeakerThread::finish`] records).
    pub(crate) fn stop_work(&self, why: &str) -> bool {
        // The end signal is sent only by a thread that ended and dropped by one that
        // crashed (taking it here is harmless: `finish` then sees the channel closed).
        if !matches!(self.done.try_recv(), Err(TryRecvError::Empty)) {
            self.request_stop();
            return false;
        }
        let mut any = false;
        for (_, shared) in &self.sides {
            let held = &shared.held;
            let running =
                !held.stopped.swap(true, Ordering::SeqCst) && !held.overflow.load(Ordering::SeqCst);
            if running {
                shared.stop(why.to_owned());
                any = true;
            }
        }
        self.request_stop();
        any
    }

    /// Stops speaker work and waits at most `wait` for the thread to end (module docs);
    /// returns each track's windows and why speaker work stopped early.
    pub(crate) fn finish(mut self, wait: Duration) -> Vec<(TrackId, SpeakerOutcome)> {
        self.request_stop();
        self.to = None;
        let ended = !matches!(self.done.recv_timeout(wait), Err(RecvTimeoutError::Timeout));
        let mut crashed = false;
        if ended && let Some(t) = self.thread.take() {
            // It sent `done` or crashed: it is ending, so this does not wait.
            crashed = t.join().is_err();
        } else {
            // Left to end on its own after its model call (module docs): from here on it
            // writes nothing (the snapshot locks below wait for a publish in progress).
            self.detached.store(true, Ordering::SeqCst);
        }
        self.sides
            .iter()
            .map(|(track, shared)| {
                let Ok(done) = shared.done.lock() else {
                    return (*track, None);
                };
                let (results, mut stopped) = done.clone();
                if stopped.is_none() && shared.held.overflow.load(Ordering::SeqCst) {
                    stopped = Some(behind_reason());
                }
                if stopped.is_none() && crashed {
                    stopped = Some("the live speaker thread crashed".to_owned());
                }
                (*track, Some((results, stopped)))
            })
            .collect()
    }
}

impl Drop for SpeakerThread {
    fn drop(&mut self) {
        // Not finished (the session failed): stop; the thread ends on its own and, if it
        // is still in a batch, publishes nothing more.
        self.detached.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        self.to = None;
    }
}

/// Why speaker work stops when the thread or a model pool thread runs at normal priority.
fn priority_reason(why: &str) -> String {
    format!("live speaker work could not run below normal priority: {why}")
}

const POOL_REFUSED: &str = "a speaker model thread could not lower its priority";

/// The thread's loop (module docs).
fn run(
    engines: &mut [Engine],
    inbox: &Receiver<(usize, Vec<Sample>)>,
    cancel: &CancelToken,
    detached: &AtomicBool,
    background: bool,
) {
    let refused = if background {
        match crate::priority::lower_current_thread() {
            Err(why) => Some(why),
            Ok(()) if crate::priority::pool_failed() => Some(POOL_REFUSED.to_owned()),
            Ok(()) => None,
        }
    } else {
        None
    };
    if let Some(why) = refused {
        for e in engines.iter_mut() {
            e.stop(priority_reason(&why));
        }
    }
    let mut next = 0usize;
    loop {
        // Wait for audio only while no batch is ready.
        let first = if engines.iter().any(|e| e.work.ready()) {
            match inbox.try_recv() {
                Ok(m) => Some(m),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match inbox.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            }
        };
        for (i, samples) in first.into_iter().chain(inbox.try_iter()) {
            let e = &mut engines[i];
            e.work.append(&samples);
            if let SpeakerWork::Running(s) = &e.work {
                e.shared
                    .held
                    .buffered
                    .store(s.buffered() as u64, Ordering::SeqCst);
            }
            e.shared
                .held
                .pending
                .fetch_sub(samples.len() as u64, Ordering::SeqCst);
        }
        if cancel.is_cancelled() {
            return;
        }
        for e in engines.iter_mut() {
            if matches!(e.work, SpeakerWork::Running(_))
                && e.shared.held.overflow.load(Ordering::SeqCst)
            {
                e.stop(behind_reason());
            }
        }
        // One batch, of the next track (round robin) that has one ready.
        let n = engines.len();
        let Some(i) = (0..n)
            .map(|k| (next + k) % n)
            .find(|&i| engines[i].work.ready())
        else {
            continue;
        };
        next = i + 1;
        let e = &mut engines[i];
        let SpeakerWork::Running(stream) = &mut e.work else {
            continue;
        };
        match e.diarizer.advance(stream, 1, cancel) {
            Ok(_) => {
                e.shared
                    .held
                    .buffered
                    .store(stream.buffered() as u64, Ordering::SeqCst);
                publish(&e.shared, e.progress.as_deref(), stream, detached);
            }
            // The end: the batch is rolled back; the final pass does the rest.
            Err(DiarError::Cancelled) if cancel.is_cancelled() => return,
            Err(err) => e.stop(format!("live speaker work failed: {err}")),
        }
        // ONNX Runtime may start pool threads while a model runs (each lowers itself
        // first): one that could not stops speaker work, also after the start.
        if background && crate::priority::pool_failed() {
            for e in engines.iter_mut() {
                if matches!(e.work, SpeakerWork::Running(_)) {
                    e.stop(priority_reason(POOL_REFUSED));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use earshot_core::diar::{
        DiarizationConfig, Diarizer, FakeEmbedder, FakeSegmenter, LocalActivity, SceneTurn,
        SegmenterInfo, SpeakerEmbedder, SpeakerSegmenter, scene_audio,
    };

    use super::*;

    fn centroids() -> Vec<Vec<f32>> {
        (0..8)
            .map(|k| (0..8).map(|i| if i == k { 1.0 } else { 0.0 }).collect())
            .collect()
    }

    fn diarizer_with(seg: Box<dyn SpeakerSegmenter + Send>) -> LiveDiarizer {
        Diarizer::new(
            seg,
            Box::new(FakeEmbedder::new("fake@1", centroids(), 0.0, 1))
                as Box<dyn SpeakerEmbedder + Send>,
            DiarizationConfig::default(),
        )
        .unwrap()
    }

    fn fake_diarizer() -> LiveDiarizer {
        diarizer_with(Box::new(FakeSegmenter::segmentation_3_0()))
    }

    /// `seconds` of one speaker.
    fn scene(seconds: u64) -> Vec<f32> {
        scene_audio(
            &[SceneTurn::new(0, 1_000, seconds * 1_000 - 2_000)],
            seconds * 1_000,
        )
        .samples
    }

    /// Sends `audio` for `track` in 10 ms packets, as the session does.
    fn send_packets(t: &SpeakerThread, track: TrackId, audio: &[f32]) {
        for p in audio.chunks(160) {
            t.send(track, p);
        }
    }

    /// Waits until `progress` reaches `n` (at most 60 s; the assertion after it decides).
    fn wait_for(progress: &AtomicUsize, n: usize) {
        let start = std::time::Instant::now();
        while progress.load(Ordering::SeqCst) < n && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(progress.load(Ordering::SeqCst) >= n, "{n} windows computed");
    }

    fn only(out: Vec<(TrackId, SpeakerOutcome)>) -> (WindowResults, Option<String>) {
        assert_eq!(out.len(), 1);
        out.into_iter().next().unwrap().1.expect("a state")
    }

    #[test]
    fn live_speaker_thread_computes_batches_as_audio_arrives() {
        let progress = Arc::new(AtomicUsize::new(0));
        let t = SpeakerThread::spawn(vec![SpeakerTrack {
            track: TrackId::Mic,
            threads: None,
            diarizer: fake_diarizer(),
            progress: Some(Arc::clone(&progress)),
            background: false,
        }])
        .unwrap();
        // 80 s: two batches of 32 windows.
        send_packets(&t, TrackId::Mic, &scene(80));
        wait_for(&progress, 64);
        let (results, stopped) = only(t.finish(Duration::from_secs(60)));
        assert_eq!(results.windows(), 64);
        assert_eq!(stopped, None);
        // The same windows as a stream fed the quantised audio at once.
        let mut d = fake_diarizer();
        let mut s = d.stream();
        s.append(&quantise_pcm16(&scene(80)));
        d.advance(&mut s, 2, &CancelToken::new()).unwrap();
        assert_eq!(s.results(), &results);
    }

    #[test]
    fn live_speaker_thread_serves_every_track() {
        let (mic, reference) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let t = SpeakerThread::spawn(vec![
            SpeakerTrack {
                track: TrackId::Mic,
                threads: None,
                diarizer: fake_diarizer(),
                progress: Some(Arc::clone(&mic)),
                background: false,
            },
            SpeakerTrack {
                track: TrackId::Reference,
                threads: None,
                diarizer: fake_diarizer(),
                progress: Some(Arc::clone(&reference)),
                background: false,
            },
        ])
        .unwrap();
        let audio = scene(80);
        for p in audio.chunks(160) {
            t.send(TrackId::Mic, p);
            t.send(TrackId::Reference, p);
        }
        wait_for(&mic, 64);
        wait_for(&reference, 64);
        let out = t.finish(Duration::from_secs(60));
        assert_eq!(
            out.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            [TrackId::Mic, TrackId::Reference]
        );
        for (_, state) in out {
            let (results, stopped) = state.unwrap();
            assert_eq!((results.windows(), stopped), (64, None));
        }
    }

    /// A segmenter whose calls signal `entered`, then hold until `release` is set (or the
    /// cancel token is tripped, if `cancellable`; at most 60 s).
    struct HoldingSegmenter {
        inner: FakeSegmenter,
        entered: Arc<AtomicUsize>,
        release: Arc<AtomicBool>,
        cancellable: bool,
        /// The thread priority seen in each call (Windows).
        priority: Arc<Mutex<Vec<i32>>>,
    }

    impl SpeakerSegmenter for HoldingSegmenter {
        fn info(&self) -> &SegmenterInfo {
            self.inner.info()
        }
        fn segment(
            &mut self,
            windows: &[&[Sample]],
            cancel: &CancelToken,
        ) -> Result<Vec<LocalActivity>, DiarError> {
            #[cfg(windows)]
            self.priority
                .lock()
                .unwrap()
                .push(crate::priority::current());
            self.entered.fetch_add(1, Ordering::SeqCst);
            let start = std::time::Instant::now();
            while !self.release.load(Ordering::SeqCst)
                && !(self.cancellable && cancel.is_cancelled())
                && start.elapsed() < Duration::from_secs(60)
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            if cancel.is_cancelled() {
                return Err(DiarError::Cancelled);
            }
            self.inner.segment(windows, cancel)
        }
    }

    struct Holding {
        thread: SpeakerThread,
        entered: Arc<AtomicUsize>,
        release: Arc<AtomicBool>,
        progress: Arc<AtomicUsize>,
        priority: Arc<Mutex<Vec<i32>>>,
    }

    fn holding(cancellable: bool, background: bool) -> Holding {
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicUsize::new(0));
        let priority = Arc::new(Mutex::new(Vec::new()));
        let seg = HoldingSegmenter {
            inner: FakeSegmenter::segmentation_3_0(),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            cancellable,
            priority: Arc::clone(&priority),
        };
        let thread = SpeakerThread::spawn(vec![SpeakerTrack {
            track: TrackId::Mic,
            threads: None,
            diarizer: diarizer_with(Box::new(seg)),
            progress: Some(Arc::clone(&progress)),
            background,
        }])
        .unwrap();
        Holding {
            thread,
            entered,
            release,
            progress,
            priority,
        }
    }

    fn wait_entered(entered: &AtomicUsize, n: usize) {
        let start = std::time::Instant::now();
        while entered.load(Ordering::SeqCst) < n && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(entered.load(Ordering::SeqCst) >= n, "batch {n} started");
    }

    #[test]
    fn live_speaker_batch_runs_on_however_much_audio_arrives() {
        // Audio never interrupts a batch (transcription does not share this thread).
        let h = holding(true, false);
        send_packets(&h.thread, TrackId::Mic, &scene(60));
        wait_entered(&h.entered, 1);
        // 100 s more while the batch is held: far more than any decode backlog bound.
        send_packets(&h.thread, TrackId::Mic, &scene(100));
        h.release.store(true, Ordering::SeqCst);
        wait_for(&h.progress, 32);
        let (results, stopped) = only(h.thread.finish(Duration::from_secs(60)));
        assert!(results.windows() >= 32);
        assert_eq!(stopped, None);
    }

    #[cfg(windows)]
    #[test]
    fn live_speaker_thread_runs_below_normal_priority() {
        // The model call of a background thread runs below normal priority, that of a
        // normal one (tests of what speaker work computes) at normal priority. Audio goes
        // in 1 s packets: a below-normal thread on a loaded host needs little time to get
        // to its first model call.
        for (background, expected) in [(true, crate::priority::BELOW_NORMAL), (false, 0)] {
            let h = holding(true, background);
            for p in scene(60).chunks(16_000) {
                h.thread.send(TrackId::Mic, p);
            }
            wait_entered(&h.entered, 1);
            let _ = h.thread.finish(Duration::from_secs(60));
            let seen = h.priority.lock().unwrap().clone();
            assert!(!seen.is_empty());
            assert!(seen.iter().all(|&p| p == expected), "{seen:?}");
        }
        // The session's own threads keep normal priority.
        assert_eq!(crate::priority::current(), 0);
    }

    #[test]
    fn live_speaker_stop_interrupts_the_batch_in_progress_and_keeps_the_earlier_ones() {
        let h = holding(true, false);
        // First batch through, the second held at the end of the meeting.
        h.release.store(true, Ordering::SeqCst);
        send_packets(&h.thread, TrackId::Mic, &scene(41));
        wait_for(&h.progress, 32);
        h.release.store(false, Ordering::SeqCst);
        send_packets(&h.thread, TrackId::Mic, &scene(40));
        wait_entered(&h.entered, 2);
        let (results, stopped) = only(h.thread.finish(Duration::from_secs(60)));
        assert_eq!(results.windows(), 32, "the interrupted batch is not kept");
        assert_eq!(stopped, None, "stopping at the end is not an early stop");
    }

    #[test]
    fn live_speaker_stop_work_interrupts_the_batch_keeps_the_earlier_ones_and_says_why() {
        // As the end's stop (the batch in progress is rolled back), but for the rest of the
        // meeting and with a reason: audio sent after it is not taken.
        let h = holding(true, false);
        h.release.store(true, Ordering::SeqCst);
        send_packets(&h.thread, TrackId::Mic, &scene(41));
        wait_for(&h.progress, 32);
        h.release.store(false, Ordering::SeqCst);
        send_packets(&h.thread, TrackId::Mic, &scene(40));
        wait_entered(&h.entered, 2);
        assert!(h.thread.stop_work("unplugged"));
        // Already stopped: nothing more to stop, the first reason stays.
        assert!(!h.thread.stop_work("again"));
        let pending = || h.thread.sides[0].1.held.pending.load(Ordering::SeqCst);
        let before = pending();
        send_packets(&h.thread, TrackId::Mic, &scene(60));
        assert!(pending() <= before, "audio after the stop is not taken");
        let (results, stopped) = only(h.thread.finish(Duration::from_secs(60)));
        assert_eq!(results.windows(), 32, "the interrupted batch is not kept");
        assert_eq!(stopped.as_deref(), Some("unplugged"));
    }

    #[test]
    fn live_speaker_stop_starts_no_batch_after_it() {
        let progress = Arc::new(AtomicUsize::new(0));
        let t = SpeakerThread::spawn(vec![SpeakerTrack {
            track: TrackId::Mic,
            threads: None,
            diarizer: fake_diarizer(),
            progress: Some(Arc::clone(&progress)),
            background: false,
        }])
        .unwrap();
        t.request_stop();
        send_packets(&t, TrackId::Mic, &scene(80));
        let (results, stopped) = only(t.finish(Duration::from_secs(60)));
        assert_eq!((results.windows(), stopped), (0, None));
        assert_eq!(progress.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn live_speaker_end_waits_at_most_the_stop_wait() {
        // The second batch is in a model call that ignores the cancel token: the end does
        // not wait for it, and keeps the first batch's windows as a prompt stop would.
        let h = holding(false, false);
        h.release.store(true, Ordering::SeqCst);
        send_packets(&h.thread, TrackId::Mic, &scene(41));
        wait_for(&h.progress, 32);
        h.release.store(false, Ordering::SeqCst);
        send_packets(&h.thread, TrackId::Mic, &scene(40));
        wait_entered(&h.entered, 2);
        let (results, stopped) = only(h.thread.finish(Duration::from_millis(50)));
        assert_eq!(
            h.progress.load(Ordering::SeqCst),
            32,
            "the end came while the second batch was still held"
        );
        assert_eq!(results.windows(), 32);
        assert_eq!(stopped, None);
        // Released afterwards, the left thread ends on its own (dropping its models and
        // progress counter) without writing anything.
        h.release.store(true, Ordering::SeqCst);
        let start = std::time::Instant::now();
        while Arc::strong_count(&h.progress) > 1 && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(Arc::strong_count(&h.progress), 1, "the left thread ended");
        assert_eq!(h.progress.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn live_speaker_thread_left_at_the_end_publishes_nothing() {
        // A batch that finishes after `finish` stopped waiting is neither snapshotted nor
        // counted.
        let mut d = fake_diarizer();
        let progress = Arc::new(AtomicUsize::new(0));
        let stream = d.stream();
        let e = Engine {
            work: SpeakerWork::Stopped,
            shared: Arc::new(Shared {
                held: Held::default(),
                done: Mutex::new((stream.results().clone(), None)),
            }),
            progress: Some(Arc::clone(&progress)),
            diarizer: fake_diarizer(),
        };
        let mut s = d.stream();
        s.append(&scene(41));
        d.advance(&mut s, 1, &CancelToken::new()).unwrap();
        let detached = AtomicBool::new(true);
        publish(&e.shared, e.progress.as_deref(), &s, &detached);
        assert_eq!(progress.load(Ordering::SeqCst), 0);
        assert_eq!(e.shared.done.lock().unwrap().0.windows(), 0);
        // Not detached: published.
        detached.store(false, Ordering::SeqCst);
        publish(&e.shared, e.progress.as_deref(), &s, &detached);
        assert_eq!(progress.load(Ordering::SeqCst), 32);
        assert_eq!(e.shared.done.lock().unwrap().0.windows(), 32);
    }

    /// A segmenter that fails every call.
    struct BrokenSegmenter(FakeSegmenter);

    impl SpeakerSegmenter for BrokenSegmenter {
        fn info(&self) -> &SegmenterInfo {
            self.0.info()
        }
        fn segment(
            &mut self,
            _windows: &[&[Sample]],
            _cancel: &CancelToken,
        ) -> Result<Vec<LocalActivity>, DiarError> {
            Err(DiarError::Backend("cpu lost".into()))
        }
    }

    #[test]
    fn live_speaker_track_stopped_by_a_model_error_gets_no_more_audio() {
        let t = SpeakerThread::spawn(vec![SpeakerTrack {
            track: TrackId::Mic,
            threads: None,
            diarizer: diarizer_with(Box::new(BrokenSegmenter(FakeSegmenter::segmentation_3_0()))),
            progress: None,
            background: false,
        }])
        .unwrap();
        send_packets(&t, TrackId::Mic, &scene(41));
        let held = &t.sides[0].1.held;
        let start = std::time::Instant::now();
        while !held.stopped.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            held.stopped.load(Ordering::SeqCst),
            "the model error stopped it"
        );
        let pending = held.pending.load(Ordering::SeqCst);
        send_packets(&t, TrackId::Mic, &scene(10));
        assert_eq!(
            held.pending.load(Ordering::SeqCst),
            pending,
            "audio after the stop is neither counted nor sent"
        );
        let (results, stopped) = only(t.finish(Duration::from_secs(60)));
        assert_eq!(results.windows(), 0);
        assert!(
            stopped.as_deref().is_some_and(|w| w.contains("cpu lost")),
            "{stopped:?}"
        );
    }

    /// A segmenter that panics in its second call.
    struct PanickingSegmenter {
        inner: FakeSegmenter,
        calls: usize,
    }

    impl SpeakerSegmenter for PanickingSegmenter {
        fn info(&self) -> &SegmenterInfo {
            self.inner.info()
        }
        fn segment(
            &mut self,
            windows: &[&[Sample]],
            cancel: &CancelToken,
        ) -> Result<Vec<LocalActivity>, DiarError> {
            self.calls += 1;
            assert!(self.calls < 2, "segmenter crashed");
            self.inner.segment(windows, cancel)
        }
    }

    #[test]
    fn live_speaker_thread_crash_keeps_the_finished_windows() {
        let progress = Arc::new(AtomicUsize::new(0));
        let t = SpeakerThread::spawn(vec![SpeakerTrack {
            track: TrackId::Mic,
            threads: None,
            diarizer: diarizer_with(Box::new(PanickingSegmenter {
                inner: FakeSegmenter::segmentation_3_0(),
                calls: 0,
            })),
            progress: Some(Arc::clone(&progress)),
            background: false,
        }])
        .unwrap();
        // 80 s: the second batch crashes the thread.
        send_packets(&t, TrackId::Mic, &scene(80));
        wait_for(&progress, 32);
        // A crashed thread drops its end signal without sending it.
        assert_eq!(
            t.done.recv_timeout(Duration::from_secs(60)),
            Err(RecvTimeoutError::Disconnected)
        );
        // Audio after the crash goes nowhere and never blocks.
        send_packets(&t, TrackId::Mic, &scene(10));
        // A later stop for the speaker detection setting keeps the crash as the reason.
        assert!(!t.stop_work("unplugged"));
        let (results, stopped) = only(t.finish(Duration::from_secs(60)));
        assert_eq!(results.windows(), 32, "the finished batch is kept");
        assert_eq!(stopped.as_deref(), Some("the live speaker thread crashed"));
    }

    #[test]
    fn live_speaker_work_stops_when_too_far_behind() {
        // The bound counts audio sent and not yet taken plus what the stream holds.
        let held = Held::default();
        assert!(held.admit(3, 10));
        held.buffered.store(4, Ordering::SeqCst);
        assert!(held.admit(3, 10), "at the cap: still on");
        assert!(!held.admit(1, 10), "beyond it: off");
        assert!(held.overflow.load(Ordering::SeqCst));
        held.pending.store(0, Ordering::SeqCst);
        held.buffered.store(0, Ordering::SeqCst);
        assert!(!held.admit(1, 10), "off stays off");

        // A thread held in its first batch while the track's audio passes the bound.
        let h = holding(true, false);
        send_packets(&h.thread, TrackId::Mic, &scene(60));
        wait_entered(&h.entered, 1);
        let cap = PIPELINE_RATE.frames_for(SPEAKER_MAX_BUFFER);
        let more = vec![0.0; 160];
        while !h.thread.sides[0].1.held.overflow.load(Ordering::SeqCst) {
            h.thread.send(TrackId::Mic, &more);
        }
        let sent_after = h.thread.sides[0].1.held.pending.load(Ordering::SeqCst)
            + h.thread.sides[0].1.held.buffered.load(Ordering::SeqCst);
        assert!(sent_after <= cap as u64, "{sent_after} held, cap {cap}");
        h.release.store(true, Ordering::SeqCst);
        let (results, stopped) = only(h.thread.finish(Duration::from_secs(60)));
        assert!(
            stopped
                .as_deref()
                .is_some_and(|w| w.contains("fell more than 180 s behind")),
            "{stopped:?}"
        );
        // The batch in progress when it stopped may or may not have finished.
        assert!(results.windows() <= 32);
    }
}
