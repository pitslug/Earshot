//! Live voice activity and transcript lag (T7.2a): the "listening" signals of the meeting
//! screen (D20, docs/ui-design.md "During the meeting"), computed on the session thread as
//! the audio is pumped.
//!
//! **Voice activity** ([`super::RuntimeEvent::VoiceActivity`]) runs core's
//! [`EnergyVad`] (an adaptive noise floor by minimum statistics, not a fixed level
//! threshold) over each track's pipeline audio: the cleaned mic ("You", after the echo
//! canceller, so far-end echo the canceller removed does not light it) and the mono
//! reference ("Them"). It is not the transcription workers' Silero VAD: those run only
//! when a worker decodes, which for speech stretches is 30-60 s behind the audio, while
//! this follows the audio as it arrives. Its settings are [`VoiceActivityConfig`]: the
//! energy VAD's defaults (30 ms frames, at least -50 dBFS and 10 dB above the floor),
//! speech for [`VoiceActivityConfig::onset`] (60 ms) to switch on, non-speech for
//! [`VoiceActivityConfig::release`] (500 ms) to switch off, so the pauses between words
//! do not make the indicator flicker. An "on" is stamped at the start of its first speech
//! frame, an "off" at the end of the last speech frame before the release; the off is
//! sent only once the release has passed, so it arrives about 500 ms after that time.
//! A track still speaking when the session ends is switched off at the end of its audio,
//! before `Ended`.
//!
//! **Transcript lag** ([`super::RuntimeEvent::TranscriptLag`]) is how much of a track's
//! audio is not yet transcribed: the audio the session sent to the track's transcription
//! worker minus the worker's decoded frontier (`LiveTrack::decoded_through`: for speech
//! stretches every speech piece before it is decoded, so silence costs nothing and an
//! open stretch counts until it is decoded; for the chunker everything fed and run). It is
//! sent once per session poll that carried a stats snapshot, so at most every
//! `PipelineConfig::stats_interval` of meeting time (1 s by default), for each track with
//! a worker. It is not `TrackStats::asr_backlog`, which counts only the samples queued for
//! the worker, not the open stretch the worker holds.
//!
//! **The far-end watch** ([`FarEndWatch`], T6.14a) reads the same voice activity: a session
//! with a reference track whose mic has been speaking for
//! [`VoiceActivityConfig::far_end_silent_after`] while the reference has spoken for less than
//! [`VoiceActivityConfig::far_end_heard_after`] sends one
//! [`super::RuntimeWarning::FarEndSilent`]; if the reference reaches that amount later it
//! sends one [`super::RuntimeWarning::FarEndHeard`], and the watch ends. The amounts are
//! judgements, not measurements: one person talking alone for a minute gets a warning that
//! does not apply, which costs less than a meeting recorded without its far end (two on
//! 2026-11-06, one on 2026-10-22). A notification sound stays below the heard amount.

use std::time::Duration;

use earshot_core::asr::{EnergyVad, EnergyVadConfig, MeetingTime, Vad};
use earshot_core::audio::{PIPELINE_RATE, Sample};

/// Settings of the live voice-activity indicator (module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceActivityConfig {
    /// The energy VAD (default: [`EnergyVadConfig::default`], 30 ms frames).
    pub vad: EnergyVadConfig,
    /// Speech this long (in whole VAD frames, rounded up) switches the indicator on
    /// (default 60 ms).
    pub onset: Duration,
    /// Non-speech this long (in whole VAD frames, rounded up) switches it off (default
    /// 500 ms).
    pub release: Duration,
    /// Mic speech with no far end heard before the far-end warning (default 60 s).
    pub far_end_silent_after: Duration,
    /// Reference speech that counts as hearing the far end (default 3 s).
    pub far_end_heard_after: Duration,
}

impl Default for VoiceActivityConfig {
    fn default() -> Self {
        Self {
            vad: EnergyVadConfig::default(),
            onset: Duration::from_millis(60),
            release: Duration::from_millis(500),
            far_end_silent_after: Duration::from_secs(60),
            far_end_heard_after: Duration::from_secs(3),
        }
    }
}

/// How much of a track's audio is not yet transcribed: `sent_frames` pipeline frames sent
/// to its worker, transcribed up to `decoded_through` (module docs). Never negative.
#[must_use]
pub fn transcript_lag(sent_frames: u64, decoded_through: MeetingTime) -> Duration {
    let heard = MeetingTime::from_pipeline_frames(sent_frames);
    Duration::from_millis(
        heard
            .as_millis()
            .saturating_sub(decoded_through.as_millis()),
    )
}

/// Total time a track has been speaking, from its voice-activity changes.
#[derive(Debug, Default)]
struct SpeakingTime {
    closed: Duration,
    open_since: Option<MeetingTime>,
}

impl SpeakingTime {
    fn change(&mut self, speaking: bool, at: MeetingTime) {
        match (speaking, self.open_since) {
            (true, None) => self.open_since = Some(at),
            (false, Some(since)) => {
                self.closed +=
                    Duration::from_millis(at.as_millis().saturating_sub(since.as_millis()));
                self.open_since = None;
            }
            _ => {}
        }
    }

    fn until(&self, now: MeetingTime) -> Duration {
        let open = self
            .open_since
            .map_or(0, |since| now.as_millis().saturating_sub(since.as_millis()));
        self.closed + Duration::from_millis(open)
    }
}

/// What the far-end watch found (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FarEndChange {
    /// The mic speaks and the far end has not been heard.
    Silent,
    /// The far end is heard after [`FarEndChange::Silent`].
    Heard,
}

/// Compares the mic's and the reference's speaking time (module docs).
#[derive(Debug)]
pub(crate) struct FarEndWatch {
    silent_after: Duration,
    heard_after: Duration,
    mic: SpeakingTime,
    far: SpeakingTime,
    warned: bool,
    done: bool,
}

impl FarEndWatch {
    pub(crate) fn new(config: &VoiceActivityConfig) -> Self {
        Self {
            silent_after: config.far_end_silent_after,
            heard_after: config.far_end_heard_after,
            mic: SpeakingTime::default(),
            far: SpeakingTime::default(),
            warned: false,
            done: false,
        }
    }

    /// A voice-activity change of the mic (`far_end` false) or the reference (true).
    pub(crate) fn change(&mut self, far_end: bool, speaking: bool, at: MeetingTime) {
        let track = if far_end {
            &mut self.far
        } else {
            &mut self.mic
        };
        track.change(speaking, at);
    }

    /// The change at meeting time `now`, at most one of each per session.
    pub(crate) fn check(&mut self, now: MeetingTime) -> Option<FarEndChange> {
        if self.done {
            return None;
        }
        if self.far.until(now) >= self.heard_after {
            self.done = true;
            return self.warned.then_some(FarEndChange::Heard);
        }
        if !self.warned && self.mic.until(now) >= self.silent_after {
            self.warned = true;
            return Some(FarEndChange::Silent);
        }
        None
    }
}

/// One track's voice-activity state.
#[derive(Debug)]
pub(crate) struct VoiceTracker {
    vad: EnergyVad,
    frame: usize,
    onset_frames: u32,
    release_frames: u32,
    /// Samples not yet a whole VAD frame.
    pending: Vec<Sample>,
    /// Pipeline frame of `pending[0]` (of the next sample when it is empty).
    next: u64,
    speaking: bool,
    /// Consecutive speech frames while off, and where that run started.
    speech_run: u32,
    run_start: u64,
    /// Consecutive non-speech frames while on.
    silence_run: u32,
    /// End of the last speech frame (pipeline frames).
    last_speech_end: u64,
}

impl VoiceTracker {
    /// A tracker with `config`.
    ///
    /// # Errors
    /// The energy VAD refuses `config.vad`.
    pub(crate) fn new(config: &VoiceActivityConfig) -> Result<Self, earshot_core::asr::AsrError> {
        let vad = EnergyVad::new(config.vad)?;
        let frame = vad.frame_len();
        let frame_time = PIPELINE_RATE.duration_of(frame);
        let frames_for = |d: Duration| -> u32 {
            let n = d.as_nanos().div_ceil(frame_time.as_nanos().max(1));
            u32::try_from(n).unwrap_or(u32::MAX).max(1)
        };
        Ok(Self {
            vad,
            frame,
            onset_frames: frames_for(config.onset),
            release_frames: frames_for(config.release),
            pending: Vec::new(),
            next: 0,
            speaking: false,
            speech_run: 0,
            run_start: 0,
            silence_run: 0,
            last_speech_end: 0,
        })
    }

    /// Adds the track's pipeline audio starting at pipeline frame `start`; returns the
    /// changes (`speaking`, when) it causes, in order. Audio that does not continue the
    /// previous call (a gap or an overlap) starts a new frame grid at `start`; the noise
    /// floor and the state are kept.
    pub(crate) fn push(&mut self, start: u64, samples: &[Sample]) -> Vec<(bool, MeetingTime)> {
        if start != self.next + self.pending.len() as u64 {
            self.pending.clear();
            self.next = start;
        }
        self.pending.extend_from_slice(samples);
        let mut changes = Vec::new();
        let whole = self.pending.len() / self.frame * self.frame;
        let pending = std::mem::take(&mut self.pending);
        for (i, frame) in pending[..whole].chunks(self.frame).enumerate() {
            let at = self.next + (i * self.frame) as u64;
            // The energy VAD only fails on a wrong frame length, which `chunks` rules out.
            let speech = self.vad.speech_probability(frame).unwrap_or(0.0) >= 0.5;
            if let Some(change) = self.step(at, speech) {
                changes.push(change);
            }
        }
        self.pending = pending;
        self.pending.drain(..whole);
        self.next += whole as u64;
        changes
    }

    fn step(&mut self, at: u64, speech: bool) -> Option<(bool, MeetingTime)> {
        let end = at + self.frame as u64;
        if self.speaking {
            if speech {
                self.silence_run = 0;
                self.last_speech_end = end;
                return None;
            }
            self.silence_run += 1;
            if self.silence_run >= self.release_frames {
                self.speaking = false;
                self.silence_run = 0;
                return Some((
                    false,
                    MeetingTime::from_pipeline_frames(self.last_speech_end),
                ));
            }
            return None;
        }
        if !speech {
            self.speech_run = 0;
            return None;
        }
        if self.speech_run == 0 {
            self.run_start = at;
        }
        self.speech_run += 1;
        self.last_speech_end = end;
        if self.speech_run >= self.onset_frames {
            self.speaking = true;
            self.speech_run = 0;
            return Some((true, MeetingTime::from_pipeline_frames(self.run_start)));
        }
        None
    }

    /// At the end of the session: an "off" at `end` if the track is still speaking.
    pub(crate) fn finish(&mut self, end: MeetingTime) -> Option<(bool, MeetingTime)> {
        if !self.speaking {
            return None;
        }
        self.speaking = false;
        Some((false, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> VoiceTracker {
        VoiceTracker::new(&VoiceActivityConfig::default()).unwrap()
    }

    /// `ms` of a constant-amplitude square wave (RMS level = 20 log10(amp) dBFS).
    fn tone(ms: u64, amp: f32) -> Vec<f32> {
        (0..ms * 16)
            .map(|i| if i % 2 == 0 { amp } else { -amp })
            .collect()
    }

    fn ms(v: u64) -> MeetingTime {
        MeetingTime::from_millis(v)
    }

    fn watch() -> FarEndWatch {
        FarEndWatch::new(&VoiceActivityConfig::default())
    }

    #[test]
    fn live_far_end_watch_warns_once_after_a_minute_of_mic_speech_alone() {
        let mut w = watch();
        w.change(false, true, ms(1_000));
        assert_eq!(w.check(ms(60_999)), None);
        assert_eq!(w.check(ms(61_000)), Some(FarEndChange::Silent));
        assert_eq!(w.check(ms(61_001)), None);
        assert_eq!(w.check(ms(600_000)), None);
    }

    #[test]
    fn live_far_end_watch_adds_up_separate_speech_and_skips_the_pauses() {
        let mut w = watch();
        w.change(false, true, ms(0));
        w.change(false, false, ms(40_000));
        // A long pause adds nothing.
        assert_eq!(w.check(ms(500_000)), None);
        w.change(false, true, ms(500_000));
        assert_eq!(w.check(ms(519_999)), None);
        assert_eq!(w.check(ms(520_000)), Some(FarEndChange::Silent));
    }

    #[test]
    fn live_far_end_watch_stays_quiet_when_the_far_end_was_heard_first() {
        let mut w = watch();
        w.change(true, true, ms(0));
        w.change(true, false, ms(3_000));
        assert_eq!(w.check(ms(3_000)), None);
        w.change(false, true, ms(4_000));
        assert_eq!(w.check(ms(4_000_000)), None);
    }

    #[test]
    fn live_far_end_watch_is_not_satisfied_by_a_notification_sound() {
        let mut w = watch();
        w.change(true, true, ms(0));
        w.change(true, false, ms(2_999));
        w.change(false, true, ms(5_000));
        assert_eq!(w.check(ms(65_000)), Some(FarEndChange::Silent));
    }

    #[test]
    fn live_far_end_watch_reports_the_far_end_arriving_after_the_warning() {
        let mut w = watch();
        w.change(false, true, ms(0));
        assert_eq!(w.check(ms(60_000)), Some(FarEndChange::Silent));
        w.change(true, true, ms(70_000));
        assert_eq!(w.check(ms(72_999)), None);
        assert_eq!(w.check(ms(73_000)), Some(FarEndChange::Heard));
        assert_eq!(w.check(ms(80_000)), None);
        // An open span from a later time never counts backwards.
        let mut late = watch();
        late.change(false, true, ms(10_000));
        assert_eq!(late.check(ms(5_000)), None);
    }

    #[test]
    fn live_transcript_lag_is_the_audio_sent_minus_the_decoded_frontier() {
        let sent = 16_000 * 65;
        assert_eq!(
            transcript_lag(sent, MeetingTime::from_millis(5_000)),
            Duration::from_secs(60)
        );
        assert_eq!(
            transcript_lag(sent, MeetingTime::from_millis(65_000)),
            Duration::ZERO
        );
        // A frontier past what was sent (the worker finished a final flush) is no lag.
        assert_eq!(
            transcript_lag(16_000, MeetingTime::from_millis(2_000)),
            Duration::ZERO
        );
        assert_eq!(transcript_lag(0, MeetingTime::ZERO), Duration::ZERO);
        // Frames convert as meeting time does (16 frames = 1 ms, to the nearest ms).
        assert_eq!(
            transcript_lag(16 * 1_500 + 15, MeetingTime::from_millis(500)),
            Duration::from_millis(1_001)
        );
        assert_eq!(
            transcript_lag(16 * 1_500 + 7, MeetingTime::from_millis(500)),
            Duration::from_millis(1_000)
        );
    }

    #[test]
    fn live_voice_activity_switches_with_onset_and_release() {
        let mut t = tracker();
        let mut changes = t.push(0, &tone(1_200, 1e-4));
        assert!(changes.is_empty(), "quiet: {changes:?}");
        changes.extend(t.push(1_200 * 16, &tone(900, 0.2)));
        // 30 ms frames: speech from the frame at 1200 ms; on after the 60 ms onset.
        assert_eq!(changes, [(true, MeetingTime::from_millis(1_200))]);
        // A 300 ms pause (shorter than the release) keeps it on.
        let mut more = t.push(2_100 * 16, &tone(300, 1e-4));
        more.extend(t.push(2_400 * 16, &tone(600, 0.2)));
        assert!(more.is_empty(), "{more:?}");
        // A long pause switches it off at the end of the last speech frame.
        let off = t.push(3_000 * 16, &tone(2_000, 1e-4));
        assert_eq!(off, [(false, MeetingTime::from_millis(3_000))]);
        assert_eq!(t.finish(MeetingTime::from_millis(5_000)), None);
    }

    #[test]
    fn live_voice_activity_ignores_a_click_shorter_than_the_onset() {
        let mut t = tracker();
        let mut changes = t.push(0, &tone(1_500, 1e-4));
        // One 30 ms frame of sound, then quiet again.
        changes.extend(t.push(1_500 * 16, &tone(30, 0.3)));
        changes.extend(t.push(1_530 * 16, &tone(1_500, 1e-4)));
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn live_voice_activity_does_not_depend_on_push_sizes_and_ends_with_the_session() {
        let audio: Vec<f32> = [tone(1_000, 1e-4), tone(800, 0.2)].concat();
        let mut whole = tracker();
        let expect = whole.push(0, &audio);
        let mut split = tracker();
        let mut got = Vec::new();
        let mut at = 0usize;
        for n in [7usize, 480, 1_000, 13, 3_333].iter().cycle() {
            if at >= audio.len() {
                break;
            }
            let end = (at + n).min(audio.len());
            got.extend(split.push(at as u64, &audio[at..end]));
            at = end;
        }
        assert_eq!(got, expect);
        assert_eq!(expect, [(true, MeetingTime::from_millis(990))]);
        // Still speaking at the end: switched off there.
        let end = MeetingTime::from_millis(1_800);
        assert_eq!(split.finish(end), Some((false, end)));
        assert_eq!(split.finish(end), None);
    }

    #[test]
    fn live_voice_activity_restarts_its_frame_grid_after_a_gap() {
        let mut t = tracker();
        assert!(t.push(0, &tone(1_000, 1e-4)).is_empty());
        // The next audio starts 5 s later (a gap the pipeline did not fill).
        let changes = t.push(6_000 * 16, &tone(200, 0.2));
        assert_eq!(changes, [(true, MeetingTime::from_millis(6_000))]);
    }
}
