//! What the app is busy with, for the one exclusion rule between the model check and the
//! rest (T7.4e): the model check measures how fast this computer
//! transcribes, so it never runs beside a meeting or a meeting job (re-transcribe,
//! re-summarise), which would distort its timings; and none of those starts while a check
//! runs. Meetings and meeting jobs may run beside each other as before.
//!
//! The "your voice" sample (T7.4h) records from the microphone, so it never runs beside a
//! meeting (either way round) or the model check (either way round); it may run beside a
//! meeting job.
//!
//! The audio check (T7.4j) plays sound and records the microphone and the output, so it
//! never runs beside a meeting, the model check or the voice sample (each either way
//! round); it may run beside a meeting job.
//!
//! The benchmark (T7.4c) measures how fast this computer runs a meeting, so like the model
//! check it never runs beside anything else: a meeting, a meeting job, the model check, the
//! voice sample or the audio check (each either way round).
//!
//! [`WorkGate`] is the one source: the meeting driver takes a [`WorkPass`] from
//! [`WorkGate::enter_meeting`] for each meeting thread, the job runner one from
//! [`WorkGate::enter_work`] for each job thread, the model jobs a [`CheckPass`] for the check
//! thread, the voice sample a [`VoicePass`], the audio check an [`AudioCheckPass`] and the
//! benchmark a [`BenchmarkPass`]; each
//! pass is held for its thread's whole
//! life and released when dropped.
//!
//! A check nobody asked for gives way to a meeting ([`CheckPass::gives_way_to_meetings`]): a
//! setting never costs a recording, so the meeting starts at once and the check is cancelled. It is
//! not held like a meeting's models (T9.1l): capture and the canceller running beside a check would
//! distort what it measures, and what it finds is stored for later meetings.
//!
//! **Nothing that waits for the CUDA start holds a pass that refuses meetings**: that start can take
//! minutes and a meeting never waits for it. So the check, the voice sample (both load ONNX Runtime)
//! and the benchmark (ONNX Runtime and llama.cpp) are refused, retryably, while it runs
//! ([`WorkGate::for_app`], [`CUDA_START_RUNNING`]), before any pass is taken. No later wait is
//! possible: the app closes those gates once, before any of this can be asked for. A meeting job's
//! pass may wait: it refuses no meeting. The audio check loads neither library.

use std::sync::{Arc, Mutex, PoisonError};

use earshot_core::asr::CancelToken;

use crate::ipc::IpcError;

/// Whether the CUDA start still has ONNX Runtime's and llama.cpp's gates closed, in that order.
type StartGates = fn() -> (bool, bool);

#[derive(Debug, Default)]
struct State {
    /// Asked before a pass that would wait at those gates; tests give their own.
    start_gates: Option<StartGates>,
    /// Meetings and meeting jobs running.
    work: usize,
    /// Meetings running (counted in `work` too).
    meetings: usize,
    /// Work after a meeting whose meeting thread may have left it (it stopped answering): it may
    /// still wake and load models, so the check, the benchmark, the voice sample, the audio
    /// check and a CUDA part's removal stay out until it ends; an update's restart and meetings
    /// do not.
    left: usize,
    /// A model check runs.
    check: bool,
    /// It stops for a meeting: its cancel token.
    check_gives_way: Option<CancelToken>,
    /// Which pass holds `check`: a check that gave way may still be ending beside a later one.
    check_pass: u64,
    /// The voice sample records or is saved.
    voice: bool,
    /// The audio check runs.
    audio_check: bool,
    /// The benchmark runs.
    benchmark: bool,
}

/// The shared busy state (cheap to clone; clones share it).
#[derive(Debug, Clone, Default)]
pub struct WorkGate(Arc<Mutex<State>>);

/// Held while a meeting or a meeting job runs.
#[derive(Debug)]
pub struct WorkPass {
    gate: WorkGate,
    kind: PassKind,
}

/// What a [`WorkPass`] counts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassKind {
    Meeting,
    Job,
    AfterMeeting,
}

/// Held while the voice sample records or is saved.
#[derive(Debug)]
pub struct VoicePass(WorkGate);

/// Held while the model check runs.
#[derive(Debug)]
pub struct CheckPass {
    gate: WorkGate,
    id: u64,
}

/// Held while the audio check runs.
#[derive(Debug)]
pub struct AudioCheckPass(WorkGate);

/// Held while the benchmark runs.
#[derive(Debug)]
pub struct BenchmarkPass(WorkGate);

/// What a meeting or a job hears while the check runs.
pub const CHECK_RUNNING: &str = "The model check is running; try again when it has finished.";
/// What the check hears while a meeting or a job runs.
pub const WORK_RUNNING: &str =
    "A meeting or a meeting job is running; run the model check when it has finished.";
/// What a meeting or the check hears while the voice sample runs.
pub const VOICE_RUNNING: &str =
    "Your voice sample is being recorded; try again when it has finished.";
/// What the voice sample hears while a meeting runs.
pub const MEETING_RUNNING: &str = "A meeting is running; record your voice when it has finished.";
/// What a meeting, the model check or the voice sample hears while the audio check runs.
pub const AUDIO_CHECK_RUNNING: &str = "The audio check is running; try again when it has finished.";
/// What the audio check hears while a meeting runs.
pub const MEETING_RUNNING_FOR_AUDIO_CHECK: &str =
    "A meeting is running; run the audio check when it has finished.";

/// What a CUDA part's removal hears while a meeting or a meeting job runs.
pub const WORK_RUNNING_FOR_CUDA_REMOVAL: &str =
    "A meeting or a meeting job is running; remove these files when it has finished.";

/// What the check, the voice sample and the benchmark hear while the CUDA start runs.
pub const CUDA_START_RUNNING: &str =
    "Earshot is still trying the CUDA libraries on your card. Try again when that has finished.";

/// What everything else hears while the benchmark runs.
pub const BENCHMARK_RUNNING: &str = "The benchmark is running; try again when it has finished.";
/// What the benchmark hears while another one runs.
pub const BENCHMARK_ALREADY_RUNNING: &str = "The benchmark is already running.";
/// What the benchmark hears while a meeting or a meeting job runs.
pub const WORK_RUNNING_FOR_BENCHMARK: &str =
    "A meeting or a meeting job is running; run the benchmark when it has finished.";

fn busy(message: &str) -> IpcError {
    IpcError {
        kind: "busy".into(),
        message: message.into(),
        retryable: true,
    }
}

impl WorkGate {
    /// A gate with nothing running.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The app's gate: refuses what would wait for the CUDA start (module docs).
    #[must_use]
    pub fn for_app() -> Self {
        Self::with_start_gates(|| {
            (
                earshot_runtime::cuda::onnx_held(),
                earshot_runtime::host::llama_held(),
            )
        })
    }

    /// A gate that asks `gates` whether the CUDA start is running.
    #[must_use]
    pub fn with_start_gates(gates: StartGates) -> Self {
        let gate = Self::default();
        gate.state().start_gates = Some(gates);
        gate
    }

    /// `busy` while the CUDA start has ONNX Runtime's gate closed, or with `llama` llama.cpp's too.
    fn refuse_during_cuda_start(s: &State, llama: bool) -> Result<(), IpcError> {
        let (onnx_held, llama_held) = s.start_gates.map_or((false, false), |gates| gates());
        if onnx_held || (llama && llama_held) {
            return Err(busy(CUDA_START_RUNNING));
        }
        Ok(())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A pass for a meeting or a meeting job.
    ///
    /// # Errors
    /// `busy` (retryable, [`CHECK_RUNNING`]) while the model check runs.
    pub fn enter_work(&self) -> Result<WorkPass, IpcError> {
        let mut s = self.state();
        if s.check {
            return Err(busy(CHECK_RUNNING));
        }
        if s.benchmark {
            return Err(busy(BENCHMARK_RUNNING));
        }
        s.work += 1;
        Ok(WorkPass {
            gate: self.clone(),
            kind: PassKind::Job,
        })
    }

    /// A pass for a meeting.
    ///
    /// # Errors
    /// `busy` (retryable) while the model check ([`CHECK_RUNNING`]), the voice sample
    /// ([`VOICE_RUNNING`]) or the audio check ([`AUDIO_CHECK_RUNNING`]) runs. A check that gives
    /// way is cancelled instead.
    pub fn enter_meeting(&self) -> Result<WorkPass, IpcError> {
        let mut s = self.state();
        if s.check {
            let Some(cancel) = s.check_gives_way.take() else {
                return Err(busy(CHECK_RUNNING));
            };
            // Over from here for the gate: its pass's drop finds another holder, or none.
            cancel.cancel();
            s.check = false;
        }
        if s.voice {
            return Err(busy(VOICE_RUNNING));
        }
        if s.audio_check {
            return Err(busy(AUDIO_CHECK_RUNNING));
        }
        if s.benchmark {
            return Err(busy(BENCHMARK_RUNNING));
        }
        s.work += 1;
        s.meetings += 1;
        Ok(WorkPass {
            gate: self.clone(),
            kind: PassKind::Meeting,
        })
    }

    /// The pass of the voice sample.
    ///
    /// # Errors
    /// `busy` (retryable) while a meeting ([`MEETING_RUNNING`]), the model check
    /// ([`CHECK_RUNNING`]), the audio check ([`AUDIO_CHECK_RUNNING`]), another voice sample or
    /// the CUDA start ([`CUDA_START_RUNNING`]) runs.
    pub fn enter_voice(&self) -> Result<VoicePass, IpcError> {
        let mut s = self.state();
        Self::refuse_during_cuda_start(&s, false)?;
        if s.meetings > 0 || s.left > 0 {
            return Err(busy(MEETING_RUNNING));
        }
        if s.check {
            return Err(busy(CHECK_RUNNING));
        }
        if s.voice {
            return Err(busy("Your voice sample is already being recorded."));
        }
        if s.audio_check {
            return Err(busy(AUDIO_CHECK_RUNNING));
        }
        if s.benchmark {
            return Err(busy(BENCHMARK_RUNNING));
        }
        s.voice = true;
        Ok(VoicePass(self.clone()))
    }

    /// The pass of the model check.
    ///
    /// # Errors
    /// `busy` (retryable) while a meeting or a job runs ([`WORK_RUNNING`]), the voice sample,
    /// the audio check or the CUDA start ([`CUDA_START_RUNNING`]) runs, or a check runs.
    pub fn enter_check(&self) -> Result<CheckPass, IpcError> {
        let mut s = self.state();
        Self::refuse_during_cuda_start(&s, false)?;
        if s.work > 0 || s.left > 0 {
            return Err(busy(WORK_RUNNING));
        }
        if s.voice {
            return Err(busy(VOICE_RUNNING));
        }
        if s.check {
            return Err(busy("The model check is already running."));
        }
        if s.audio_check {
            return Err(busy(AUDIO_CHECK_RUNNING));
        }
        if s.benchmark {
            return Err(busy(BENCHMARK_RUNNING));
        }
        s.check = true;
        s.check_pass += 1;
        Ok(CheckPass {
            gate: self.clone(),
            id: s.check_pass,
        })
    }

    /// The pass of a CUDA part's removal, held for the moments it takes: a meeting or a job may
    /// have the files mapped, and none starts meanwhile (it hears [`CHECK_RUNNING`], as beside
    /// the check, whose slot this holds).
    ///
    /// # Errors
    /// `busy` (retryable) while a meeting or a job ([`WORK_RUNNING_FOR_CUDA_REMOVAL`]) or
    /// anything else that holds the gate runs.
    pub fn enter_cuda_removal(&self) -> Result<CheckPass, IpcError> {
        // Work after a meeting that was left may still have the files mapped.
        let busy_work = {
            let s = self.state();
            s.work > 0 || s.left > 0
        };
        if busy_work {
            return Err(busy(WORK_RUNNING_FOR_CUDA_REMOVAL));
        }
        self.enter_check()
    }

    /// The pass of the audio check.
    ///
    /// # Errors
    /// `busy` (retryable) while a meeting ([`MEETING_RUNNING_FOR_AUDIO_CHECK`]), the model
    /// check ([`CHECK_RUNNING`]), the voice sample ([`VOICE_RUNNING`]) or another audio check
    /// runs.
    pub fn enter_audio_check(&self) -> Result<AudioCheckPass, IpcError> {
        let mut s = self.state();
        if s.meetings > 0 || s.left > 0 {
            return Err(busy(MEETING_RUNNING_FOR_AUDIO_CHECK));
        }
        if s.check {
            return Err(busy(CHECK_RUNNING));
        }
        if s.voice {
            return Err(busy(VOICE_RUNNING));
        }
        if s.audio_check {
            return Err(busy("The audio check is already running."));
        }
        if s.benchmark {
            return Err(busy(BENCHMARK_RUNNING));
        }
        s.audio_check = true;
        Ok(AudioCheckPass(self.clone()))
    }

    /// The pass of the benchmark.
    ///
    /// # Errors
    /// `busy` (retryable) while a meeting or a meeting job ([`WORK_RUNNING_FOR_BENCHMARK`]),
    /// the model check ([`CHECK_RUNNING`]), the voice sample ([`VOICE_RUNNING`]), the audio
    /// check ([`AUDIO_CHECK_RUNNING`]) or another benchmark runs.
    pub fn enter_benchmark(&self) -> Result<BenchmarkPass, IpcError> {
        {
            let s = self.state();
            Self::refuse_during_cuda_start(&s, true)?;
            if s.left > 0 {
                return Err(busy(WORK_RUNNING_FOR_BENCHMARK));
            }
        }
        self.enter_restart()
    }

    /// The benchmark's pass without looking at the CUDA start, which a restart simply ends: taken
    /// for good by a restart, so nothing long starts before the process's end.
    ///
    /// # Errors
    /// As [`Self::enter_benchmark`], but for the CUDA start.
    pub fn enter_restart(&self) -> Result<BenchmarkPass, IpcError> {
        let mut s = self.state();
        if s.work > 0 {
            return Err(busy(WORK_RUNNING_FOR_BENCHMARK));
        }
        if s.check {
            return Err(busy(CHECK_RUNNING));
        }
        if s.voice {
            return Err(busy(VOICE_RUNNING));
        }
        if s.audio_check {
            return Err(busy(AUDIO_CHECK_RUNNING));
        }
        if s.benchmark {
            return Err(busy(BENCHMARK_ALREADY_RUNNING));
        }
        s.benchmark = true;
        Ok(BenchmarkPass(self.clone()))
    }
}

impl WorkPass {
    /// The pass of a meeting's work after the meeting, beside this meeting's own: its thread may
    /// outlive the meeting's when left for stopping answering, and may still wake and load
    /// models, so it keeps the model check, the benchmark, the voice sample, the audio check and
    /// a CUDA part's removal (it may have the files mapped) out until it ends, but not an
    /// update's restart, which ends the process anyway. Never refused: while this pass is held,
    /// nothing it keeps out can be running.
    #[must_use]
    pub fn after_meeting_beside(&self) -> WorkPass {
        self.gate.state().left += 1;
        WorkPass {
            gate: self.gate.clone(),
            kind: PassKind::AfterMeeting,
        }
    }
}

impl Drop for WorkPass {
    fn drop(&mut self) {
        let mut s = self.gate.state();
        match self.kind {
            PassKind::Meeting => {
                s.work = s.work.saturating_sub(1);
                s.meetings = s.meetings.saturating_sub(1);
            }
            PassKind::Job => s.work = s.work.saturating_sub(1),
            PassKind::AfterMeeting => s.left = s.left.saturating_sub(1),
        }
    }
}

impl Drop for VoicePass {
    fn drop(&mut self) {
        self.0.state().voice = false;
    }
}

impl CheckPass {
    /// A meeting that starts now cancels this check through `cancel` rather than being refused,
    /// and the gate counts it as over at once: for a check the user did not ask for. Its process
    /// dies within moments of the meeting's start.
    ///
    /// A pass whose check is already over for the gate changes nothing: the token would belong to
    /// no running check, and a later check's meeting would cancel it instead of its own.
    pub fn gives_way_to_meetings(&self, cancel: CancelToken) {
        let mut s = self.gate.state();
        if s.check && s.check_pass == self.id {
            s.check_gives_way = Some(cancel);
        }
    }
}

impl Drop for CheckPass {
    fn drop(&mut self) {
        let mut s = self.gate.state();
        if s.check && s.check_pass == self.id {
            s.check = false;
            s.check_gives_way = None;
        }
    }
}

impl Drop for AudioCheckPass {
    fn drop(&mut self) {
        self.0.state().audio_check = false;
    }
}

impl Drop for BenchmarkPass {
    fn drop(&mut self) {
        self.0.state().benchmark = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_running() -> (bool, bool) {
        (true, true)
    }

    fn only_summaries_starting() -> (bool, bool) {
        (false, true)
    }

    /// Nothing that would wait for the CUDA start takes a pass that refuses meetings.
    #[test]
    fn activity_what_would_wait_for_the_cuda_start_is_refused_and_a_meeting_is_not() {
        let gate = WorkGate::with_start_gates(start_running);
        for e in [
            gate.enter_check().unwrap_err(),
            gate.enter_voice().unwrap_err(),
            gate.enter_benchmark().unwrap_err(),
        ] {
            assert_eq!(
                (e.kind.as_str(), e.message.as_str(), e.retryable),
                ("busy", CUDA_START_RUNNING, true)
            );
        }
        // Nothing was taken by those refusals; the audio check and a job wait for neither.
        drop(gate.enter_audio_check().unwrap());
        drop(gate.enter_work().unwrap());
        drop(gate.enter_meeting().unwrap());
        // A restart ends the start rather than waiting for it.
        drop(gate.enter_restart().unwrap());

        // Only llama.cpp's gate closed: ONNX Runtime's loaders go, the benchmark needs both.
        let gate = WorkGate::with_start_gates(only_summaries_starting);
        drop(gate.enter_check().unwrap());
        drop(gate.enter_voice().unwrap());
        assert_eq!(
            gate.enter_benchmark().unwrap_err().message,
            CUDA_START_RUNNING
        );
    }

    /// A check that gave way is over for the gate at once, and its late drop touches nothing.
    #[test]
    fn activity_a_check_that_gives_way_is_over_when_the_meeting_starts() {
        let gate = WorkGate::new();
        let old = gate.enter_check().unwrap();
        let cancel = CancelToken::new();
        old.gives_way_to_meetings(cancel.clone());
        let meeting = gate.enter_meeting().unwrap();
        assert!(cancel.is_cancelled());
        assert!(!gate.state().check);
        assert!(gate.state().check_gives_way.is_none());
        drop(meeting);
        // The old check's thread is still ending when a new one takes the gate.
        let new = gate.enter_check().unwrap();
        drop(old);
        assert_eq!(gate.enter_meeting().unwrap_err().message, CHECK_RUNNING);
        drop(new);
        drop(gate.enter_meeting().unwrap());
    }

    /// An old pass cannot mark a newer check as one that gives way: that check still refuses.
    #[test]
    fn activity_a_finished_pass_cannot_make_a_later_check_give_way() {
        let gate = WorkGate::new();
        let old = gate.enter_check().unwrap();
        old.gives_way_to_meetings(CancelToken::new());
        drop(gate.enter_meeting().unwrap());
        let new = gate.enter_check().unwrap();
        let stale = CancelToken::new();
        old.gives_way_to_meetings(stale.clone());
        assert_eq!(gate.enter_meeting().unwrap_err().message, CHECK_RUNNING);
        assert!(!stale.is_cancelled());
        drop(new);
    }

    #[test]
    fn activity_check_and_work_exclude_each_other() {
        let gate = WorkGate::new();
        let meeting = gate.enter_work().unwrap();
        let job = gate.enter_work().unwrap();
        let e = gate.enter_check().unwrap_err();
        assert_eq!((e.kind.as_str(), e.retryable), ("busy", true));
        assert_eq!(e.message, WORK_RUNNING);
        drop(meeting);
        assert!(gate.enter_check().is_err(), "a job still runs");
        drop(job);
        let check = gate.enter_check().unwrap();
        let e = gate.enter_work().unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("busy", CHECK_RUNNING)
        );
        assert!(e.retryable);
        assert_eq!(gate.enter_check().unwrap_err().kind, "busy");
        drop(check);
        drop(gate.enter_work().unwrap());
        drop(gate.enter_check().unwrap());
    }

    #[test]
    fn activity_cuda_removal_and_work_exclude_each_other() {
        let gate = WorkGate::new();
        let meeting = gate.enter_meeting().unwrap();
        let e = gate.enter_cuda_removal().unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str(), e.retryable),
            ("busy", WORK_RUNNING_FOR_CUDA_REMOVAL, true)
        );
        drop(meeting);
        let removal = gate.enter_cuda_removal().unwrap();
        assert_eq!(gate.enter_meeting().unwrap_err().message, CHECK_RUNNING);
        assert_eq!(gate.enter_work().unwrap_err().message, CHECK_RUNNING);
        drop(removal);
        drop(gate.enter_meeting().unwrap());
    }

    #[test]
    fn activity_voice_sample_excludes_meetings_and_the_check_but_not_jobs() {
        let gate = WorkGate::new();
        // A job may run beside the voice sample, either way round.
        let job = gate.enter_work().unwrap();
        let voice = gate.enter_voice().unwrap();
        let e = gate.enter_meeting().unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("busy", VOICE_RUNNING)
        );
        assert!(e.retryable);
        assert_eq!(gate.enter_voice().unwrap_err().kind, "busy");
        drop(job);
        let e = gate.enter_check().unwrap_err();
        assert_eq!(e.message, VOICE_RUNNING);
        drop(gate.enter_work().unwrap());
        drop(voice);
        // A meeting refuses the voice sample until it ends; a job no longer counts.
        let meeting = gate.enter_meeting().unwrap();
        let e = gate.enter_voice().unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("busy", MEETING_RUNNING)
        );
        assert_eq!(gate.enter_check().unwrap_err().message, WORK_RUNNING);
        drop(meeting);
        let check = gate.enter_check().unwrap();
        assert_eq!(gate.enter_voice().unwrap_err().message, CHECK_RUNNING);
        assert_eq!(gate.enter_meeting().unwrap_err().message, CHECK_RUNNING);
        drop(check);
        let job = gate.enter_work().unwrap();
        drop(gate.enter_voice().unwrap());
        drop(job);
        drop(gate.enter_meeting().unwrap());
        drop(gate.enter_voice().unwrap());
    }

    #[test]
    fn activity_audio_check_excludes_meetings_the_check_and_the_voice_sample() {
        let gate = WorkGate::new();
        // A meeting job may run beside it, either way round.
        let job = gate.enter_work().unwrap();
        let audio = gate.enter_audio_check().unwrap();
        drop(gate.enter_work().unwrap());
        drop(job);
        for e in [
            gate.enter_meeting().unwrap_err(),
            gate.enter_check().unwrap_err(),
            gate.enter_voice().unwrap_err(),
        ] {
            assert_eq!(
                (e.kind.as_str(), e.message.as_str(), e.retryable),
                ("busy", AUDIO_CHECK_RUNNING, true)
            );
        }
        assert_eq!(gate.enter_audio_check().unwrap_err().kind, "busy");
        drop(audio);
        // Each of them refuses the audio check while it runs.
        let meeting = gate.enter_meeting().unwrap();
        assert_eq!(
            gate.enter_audio_check().unwrap_err().message,
            MEETING_RUNNING_FOR_AUDIO_CHECK
        );
        drop(meeting);
        let check = gate.enter_check().unwrap();
        assert_eq!(gate.enter_audio_check().unwrap_err().message, CHECK_RUNNING);
        drop(check);
        let voice = gate.enter_voice().unwrap();
        assert_eq!(gate.enter_audio_check().unwrap_err().message, VOICE_RUNNING);
        drop(voice);
        let audio = gate.enter_audio_check().unwrap();
        drop(audio);
        drop(gate.enter_meeting().unwrap());
        drop(gate.enter_check().unwrap());
        drop(gate.enter_voice().unwrap());
    }

    #[test]
    fn activity_benchmark_excludes_everything_both_ways() {
        let gate = WorkGate::new();
        let bench = gate.enter_benchmark().unwrap();
        for e in [
            gate.enter_meeting().unwrap_err(),
            gate.enter_work().unwrap_err(),
            gate.enter_check().unwrap_err(),
            gate.enter_voice().unwrap_err(),
            gate.enter_audio_check().unwrap_err(),
        ] {
            assert_eq!(
                (e.kind.as_str(), e.message.as_str(), e.retryable),
                ("busy", BENCHMARK_RUNNING, true)
            );
        }
        assert_eq!(
            gate.enter_benchmark().unwrap_err().message,
            BENCHMARK_ALREADY_RUNNING
        );
        drop(bench);
        // Each of them refuses the benchmark while it runs.
        let meeting = gate.enter_meeting().unwrap();
        assert_eq!(
            gate.enter_benchmark().unwrap_err().message,
            WORK_RUNNING_FOR_BENCHMARK
        );
        drop(meeting);
        let job = gate.enter_work().unwrap();
        assert_eq!(
            gate.enter_benchmark().unwrap_err().message,
            WORK_RUNNING_FOR_BENCHMARK
        );
        drop(job);
        let check = gate.enter_check().unwrap();
        assert_eq!(gate.enter_benchmark().unwrap_err().message, CHECK_RUNNING);
        drop(check);
        let voice = gate.enter_voice().unwrap();
        assert_eq!(gate.enter_benchmark().unwrap_err().message, VOICE_RUNNING);
        drop(voice);
        let audio = gate.enter_audio_check().unwrap();
        assert_eq!(
            gate.enter_benchmark().unwrap_err().message,
            AUDIO_CHECK_RUNNING
        );
        drop(audio);
        drop(gate.enter_benchmark().unwrap());
        drop(gate.enter_meeting().unwrap());
    }
}
