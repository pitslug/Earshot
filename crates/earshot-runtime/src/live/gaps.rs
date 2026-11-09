//! Gaps in the recording (T9.16): the mic stopped delivering audio, and the pipeline stored the
//! time as silence. The usual cause is the computer going to sleep with the meeting on, which the
//! capture adapter's power request (`KeepAwake`) prevents only for idle sleep: a closed lid or the
//! power button still sleeps. The speech in a gap is lost, so the user is told.
//!
//! Only the mic counts. A loopback delivers nothing while nothing plays, so silence filled on the
//! reference is normal. The mic's own fill is read from the pipeline's counters: a gap is filled at
//! once when capture resumes, or a little at a time while the reference runs on without the mic,
//! so a run of polls that each add some fill is one gap.

use std::time::Duration;

use earshot_core::audio::PIPELINE_RATE;
use earshot_core::pipeline::TrackStats;

/// The shortest run of filled mic audio reported (a judgement: well above the pipeline's
/// `max_track_lag`, so the silence filled at the end of a session never counts, and long enough
/// for a person to care).
pub const REPORTED_GAP: Duration = Duration::from_secs(2);

/// Follows the mic's filled silence and adds up what is reported.
#[derive(Debug, Default)]
pub(super) struct GapWatch {
    /// `zero_filled` at the last poll.
    seen: u64,
    /// Filled in the current run of polls, reported or not.
    run: u64,
    /// Filled samples already counted as lost.
    lost: u64,
    /// Lost to timestamp jumps, which are not filled.
    jumped: Duration,
}

impl GapWatch {
    /// The total lost when this poll of the mic's counters adds to it.
    pub(super) fn stats(&mut self, mic: &TrackStats) -> Option<Duration> {
        let added = mic.zero_filled.saturating_sub(self.seen);
        self.seen = mic.zero_filled;
        if added == 0 {
            self.run = 0;
            return None;
        }
        let threshold = PIPELINE_RATE.frames_for(REPORTED_GAP) as u64;
        let before = self.run;
        self.run += added;
        if self.run < threshold {
            return None;
        }
        // The run's first report counts everything it held so far.
        self.lost += if before < threshold { self.run } else { added };
        Some(self.total())
    }

    /// A forward timestamp jump on the mic: too long to fill, so the recording closes up over it.
    /// Returns the total lost.
    pub(super) fn jump(&mut self, gap: Duration) -> Duration {
        self.jumped += gap;
        self.total()
    }

    /// Everything reported so far.
    pub(super) fn total(&self) -> Duration {
        let filled = usize::try_from(self.lost).unwrap_or(usize::MAX);
        PIPELINE_RATE.duration_of(filled) + self.jumped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(zero_filled: u64) -> TrackStats {
        TrackStats {
            zero_filled,
            ..TrackStats::default()
        }
    }

    const SECOND: u64 = 16_000;

    #[test]
    fn live_recording_gap_watch_reports_a_gap_filled_at_once() {
        let mut w = GapWatch::default();
        assert_eq!(w.stats(&at(0)), None);
        assert_eq!(w.stats(&at(40 * SECOND)), Some(Duration::from_secs(40)));
        assert_eq!(w.stats(&at(40 * SECOND)), None);
        assert_eq!(w.total(), Duration::from_secs(40));
    }

    #[test]
    fn live_recording_gap_watch_ignores_short_fills_however_many() {
        let mut w = GapWatch::default();
        let mut filled = 0;
        for _ in 0..50 {
            filled += SECOND;
            assert_eq!(w.stats(&at(filled)), None, "one second");
            assert_eq!(w.stats(&at(filled)), None, "then none");
        }
        assert_eq!(w.total(), Duration::ZERO);
        // Exactly at the limit counts; just under does not.
        let mut w = GapWatch::default();
        assert_eq!(w.stats(&at(2 * SECOND - 1)), None);
        let mut w = GapWatch::default();
        assert_eq!(w.stats(&at(2 * SECOND)), Some(Duration::from_secs(2)));
    }

    #[test]
    fn live_recording_gap_watch_joins_a_gap_filled_a_little_at_a_time() {
        let mut w = GapWatch::default();
        assert_eq!(w.stats(&at(SECOND)), None);
        assert_eq!(w.stats(&at(2 * SECOND)), Some(Duration::from_secs(2)));
        assert_eq!(w.stats(&at(3 * SECOND)), Some(Duration::from_secs(3)));
        assert_eq!(w.stats(&at(3 * SECOND)), None);
    }

    #[test]
    fn live_recording_gap_watch_adds_up_separate_gaps_and_jumps() {
        let mut w = GapWatch::default();
        assert_eq!(w.stats(&at(40 * SECOND)), Some(Duration::from_secs(40)));
        assert_eq!(w.stats(&at(40 * SECOND)), None);
        assert_eq!(w.stats(&at(97 * SECOND)), Some(Duration::from_secs(97)));
        assert_eq!(
            w.jump(Duration::from_secs(600)),
            Duration::from_secs(697),
            "a sleep past the pipeline's max_gap is not filled, and is still lost"
        );
    }
}
