//! When to offer to stop a recording because the call seems to be over, and when to stop it (T9.4).
//!
//! [`CallEndWatch`] says when to show or take back the question, and when a question nobody answered
//! has been open for [`CallEndConfig::stop_after`] ([`CallEndChange::Stop`]; the owner: a
//! recording that ran on after a call is a privacy problem, and a stop this late needs the call to
//! look over for minutes on end). It stops nothing itself: the host does. A host runs it only for a
//! call with a [`CallSignal`]: an in-room meeting or a replay has none.
//!
//! The offer needs both of these at once, for [`CallEndConfig::quiet_after`]:
//! - **The call app's session ended**: one of its render sessions was seen active during this
//!   meeting, and then [`CallEndConfig::polls_to_end`] readings in a row found none. A meeting that
//!   never saw one (a call app Earshot does not know, a call that has not started) is never offered,
//!   and a reading that failed is [`CallSessions::Unknown`], which counts as no evidence and ends the
//!   run of readings.
//! - **The far end is quiet**: no voice activity on the reference track. Silence alone never offers
//!   (hold, a break, waiting for someone to join), and neither does a session ending while the far
//!   end still sounds.
//!
//! The offer is taken back when the session returns or the far end speaks, and comes back when
//! both conditions hold again. "Keep recording" settles it until the call app's session is active
//! again: only a new session ending can ask again. Within one cycle (from a session active until
//! the next, or "Keep recording") the first offer is [`CallEndChange::Offer`] and any return of it
//! is [`CallEndChange::OfferAgain`]. The stop's wait starts at each offer: whatever takes the
//! question back, a failed reading included, starts it over, and "Keep recording" means no stop until
//! a new session has ended.
//!
//! **Judgements, not measurements** (see the default of each setting): the hang-up measurement of
//! PLAN T9.4 has not been made. The rule assumes a call app's session stops being active at
//! hang-up, which was seen only as "active in a test call, none with no call" (private notes).
//! If a call app keeps its session active after hang-up the offer and the stop simply never come;
//! nothing is lost. If a session drops during a live call and the far end stays silent throughout,
//! the stop would cut the meeting short. With whole-output capture any other sound on the output
//! counts as the far end, so music keeps the offer and the stop away.

use std::time::Duration;

/// One reading of the call app's render sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSessions {
    /// A known call app has an active render session.
    Active,
    /// None has.
    NoneActive,
    /// The sessions could not be read.
    Unknown,
}

/// The newest reading since the last call (`None`: nothing new). A host's poller stands behind it;
/// a meeting without one has no offer. Dropping it stops the polling.
pub type CallSignal = Box<dyn FnMut() -> Option<CallSessions> + Send>;

/// The rule's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallEndConfig {
    /// Readings in a row with no active session before it counts as ended (default 2, as T6.15's
    /// two-poll rule: a judgement, so one bad reading says nothing).
    pub polls_to_end: u32,
    /// How long the session must stay ended and the far end quiet (default 30 s: a judgement, not
    /// measured. Long enough for a call app to reconnect or move between sessions; short enough to
    /// be useful. Replace it once the hang-up measurement exists).
    pub quiet_after: Duration,
    /// How long a question may stay open and unanswered before the recording stops (default 3 min:
    /// The owner's choice, between the plan's 60 s and 5 min; not measured).
    pub stop_after: Duration,
    /// How old the newest session reading may be for a stop (default 10 s, five polls: a judgement).
    /// A poller that went quiet says nothing about the call, so its last word cannot stop a meeting.
    pub stop_needs_reading_within: Duration,
}

impl Default for CallEndConfig {
    fn default() -> Self {
        Self {
            polls_to_end: 2,
            quiet_after: Duration::from_secs(30),
            stop_after: Duration::from_secs(180),
            stop_needs_reading_within: Duration::from_secs(10),
        }
    }
}

/// What to do with the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEndChange {
    /// Ask whether to stop, for the first time since a session was last active.
    Offer,
    /// Ask again after a [`CallEndChange::Withdraw`], in the same cycle.
    OfferAgain,
    /// The call looks alive again: take the question away.
    Withdraw,
    /// Nobody answered and the call still looks over: stop the recording. At most once in a
    /// meeting, and nothing follows it.
    Stop,
}

/// The rule (module docs). Times are from any one clock, given by the caller.
#[derive(Debug, Clone)]
pub struct CallEndWatch {
    config: CallEndConfig,
    /// A session was active in this meeting and has not been answered with "Keep recording" since.
    armed: bool,
    /// Readings in a row with no active session.
    none_in_a_row: u32,
    /// When the session counted as ended.
    ended_at: Option<Duration>,
    far_speaking: bool,
    /// When the far end last stopped speaking.
    far_quiet_since: Duration,
    offered: bool,
    /// Already asked since a session was last active or the user last answered.
    asked: bool,
    /// When the open question was asked.
    offered_at: Duration,
    /// [`CallEndChange::Stop`] was given: nothing follows it.
    stopped: bool,
    /// When the newest session reading came.
    last_reading: Duration,
}

impl CallEndWatch {
    /// A watch at the start of a meeting.
    #[must_use]
    pub fn new(config: CallEndConfig) -> Self {
        Self {
            config,
            armed: false,
            none_in_a_row: 0,
            ended_at: None,
            far_speaking: false,
            far_quiet_since: Duration::ZERO,
            offered: false,
            asked: false,
            offered_at: Duration::ZERO,
            stopped: false,
            last_reading: Duration::ZERO,
        }
    }

    /// A reading of the sessions at `now`.
    pub fn sessions(&mut self, reading: CallSessions, now: Duration) {
        self.last_reading = now;
        match reading {
            CallSessions::Active => {
                self.armed = true;
                self.none_in_a_row = 0;
                self.ended_at = None;
                self.asked = false;
            }
            CallSessions::NoneActive => {
                self.none_in_a_row = self.none_in_a_row.saturating_add(1);
                if self.armed
                    && self.ended_at.is_none()
                    && self.none_in_a_row >= self.config.polls_to_end
                {
                    self.ended_at = Some(now);
                }
            }
            // No evidence either way: an offer needs an unbroken run of real readings.
            CallSessions::Unknown => {
                self.none_in_a_row = 0;
                self.ended_at = None;
            }
        }
    }

    /// The far end's voice activity changed at `now`.
    pub fn far_end(&mut self, speaking: bool, now: Duration) {
        self.far_speaking = speaking;
        if !speaking {
            self.far_quiet_since = now;
        }
    }

    /// The user answered "Keep recording": no question until a session is active and ends again.
    pub fn keep_recording(&mut self) {
        self.armed = false;
        self.ended_at = None;
        self.offered = false;
        self.asked = false;
    }

    /// What changes at `now`, if anything.
    pub fn check(&mut self, now: Duration) -> Option<CallEndChange> {
        if self.stopped {
            return None;
        }
        let call_over = self.ended_at.is_some_and(|ended| {
            !self.far_speaking
                && now.saturating_sub(ended.max(self.far_quiet_since)) >= self.config.quiet_after
        });
        if self.offered {
            let alive = self.ended_at.is_none() || self.far_speaking;
            if alive {
                self.offered = false;
                return Some(CallEndChange::Withdraw);
            }
            let reading_is_recent =
                now.saturating_sub(self.last_reading) <= self.config.stop_needs_reading_within;
            if reading_is_recent && now.saturating_sub(self.offered_at) >= self.config.stop_after {
                self.stopped = true;
                return Some(CallEndChange::Stop);
            }
            return None;
        }
        if call_over {
            self.offered = true;
            self.offered_at = now;
            let again = std::mem::replace(&mut self.asked, true);
            return Some(if again {
                CallEndChange::OfferAgain
            } else {
                CallEndChange::Offer
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn watch() -> CallEndWatch {
        CallEndWatch::new(CallEndConfig::default())
    }

    /// A call whose session was seen at 0 s and read as gone at 10 and 12 s (counted ended at 12).
    fn ended_at_12() -> CallEndWatch {
        let mut w = watch();
        w.sessions(CallSessions::Active, s(0));
        w.sessions(CallSessions::NoneActive, s(10));
        w.sessions(CallSessions::NoneActive, s(12));
        w
    }

    #[test]
    fn call_end_session_ended_then_quiet_offers_after_the_wait() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(41)), None);
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        // Asked once.
        assert_eq!(w.check(s(43)), None);
        assert_eq!(w.check(s(221)), None);
    }

    #[test]
    fn call_end_an_unanswered_offer_stops_once_after_the_wait() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        assert_eq!(w.check(s(221)), None);
        w.sessions(CallSessions::NoneActive, s(222));
        assert_eq!(w.check(s(222)), Some(CallEndChange::Stop));
        // Once, and nothing after it, whatever the readings say.
        assert_eq!(w.check(s(223)), None);
        w.far_end(true, s(224));
        assert_eq!(w.check(s(225)), None);
        w.sessions(CallSessions::Active, s(226));
        assert_eq!(w.check(s(4_000)), None);
    }

    #[test]
    fn call_end_the_call_looking_alive_starts_the_stop_wait_over() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        // The far end speaks one second before the stop.
        w.far_end(true, s(221));
        assert_eq!(w.check(s(221)), Some(CallEndChange::Withdraw));
        w.far_end(false, s(222));
        assert_eq!(w.check(s(252)), Some(CallEndChange::OfferAgain));
        // A whole new wait, from the new offer.
        w.sessions(CallSessions::NoneActive, s(431));
        assert_eq!(w.check(s(431)), None);
        assert_eq!(w.check(s(432)), Some(CallEndChange::Stop));

        // The session returning does the same, and with no new ending there is no stop at all.
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        w.sessions(CallSessions::Active, s(100));
        assert_eq!(w.check(s(100)), Some(CallEndChange::Withdraw));
        assert_eq!(w.check(s(86_400)), None);

        // So does a reading that failed: no evidence, no stop.
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        w.sessions(CallSessions::Unknown, s(100));
        assert_eq!(w.check(s(100)), Some(CallEndChange::Withdraw));
        assert_eq!(w.check(s(86_400)), None);
    }

    #[test]
    fn call_end_a_poller_gone_quiet_never_stops_the_recording() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        // No reading since 12 s: the call could have come back unseen.
        assert_eq!(w.check(s(222)), None);
        assert_eq!(w.check(s(86_400)), None);
        // At the limit it still counts, and one second past it does not.
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        w.sessions(CallSessions::NoneActive, s(212));
        assert_eq!(w.check(s(223)), None);
        assert_eq!(w.check(s(222)), Some(CallEndChange::Stop));
        // A reading coming back with the call still over is evidence again.
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        w.sessions(CallSessions::NoneActive, s(5_000));
        assert_eq!(w.check(s(5_000)), Some(CallEndChange::Stop));
    }

    #[test]
    fn call_end_keep_recording_means_no_stop_until_a_new_session_ends() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        w.keep_recording();
        for t in (44..86_400).step_by(60) {
            w.sessions(CallSessions::NoneActive, s(t));
            assert_eq!(w.check(s(t)), None, "at {t} s");
        }
        // Silence with the session active and a meeting with no session never stop either.
        let mut w = watch();
        w.sessions(CallSessions::Active, s(0));
        assert_eq!(w.check(s(86_400)), None);
        let mut w = watch();
        w.sessions(CallSessions::NoneActive, s(0));
        w.sessions(CallSessions::NoneActive, s(2));
        assert_eq!(w.check(s(86_400)), None);
    }

    #[test]
    fn call_end_silence_with_the_session_still_active_never_offers() {
        let mut w = watch();
        w.sessions(CallSessions::Active, s(0));
        w.far_end(false, s(1));
        for t in (2..7_200).step_by(2) {
            w.sessions(CallSessions::Active, s(t));
            assert_eq!(w.check(s(t)), None, "at {t} s");
        }
    }

    #[test]
    fn call_end_a_meeting_that_never_saw_a_session_never_offers() {
        let mut w = watch();
        for t in (0..7_200).step_by(2) {
            w.sessions(CallSessions::NoneActive, s(t));
            assert_eq!(w.check(s(t)), None, "at {t} s");
        }
    }

    #[test]
    fn call_end_one_reading_without_a_session_is_not_enough() {
        let mut w = watch();
        w.sessions(CallSessions::Active, s(0));
        w.sessions(CallSessions::NoneActive, s(2));
        assert_eq!(w.check(s(1_000)), None);
        // Back again: the run starts over.
        w.sessions(CallSessions::Active, s(1_002));
        w.sessions(CallSessions::NoneActive, s(1_004));
        assert_eq!(w.check(s(2_000)), None);
    }

    #[test]
    fn call_end_an_unreadable_session_list_is_no_evidence() {
        let mut w = watch();
        w.sessions(CallSessions::Active, s(0));
        for t in (2..1_000).step_by(2) {
            w.sessions(CallSessions::Unknown, s(t));
            assert_eq!(w.check(s(t)), None, "at {t} s");
        }
        // Readings broken by a failure do not add up, and the wait starts over.
        let mut w = ended_at_12();
        w.sessions(CallSessions::Unknown, s(20));
        w.sessions(CallSessions::NoneActive, s(22));
        assert_eq!(w.check(s(100)), None);
        w.sessions(CallSessions::NoneActive, s(104));
        assert_eq!(w.check(s(133)), None);
        assert_eq!(w.check(s(134)), Some(CallEndChange::Offer));
    }

    #[test]
    fn call_end_the_far_end_speaking_after_the_session_ended_holds_the_offer() {
        let mut w = ended_at_12();
        w.far_end(true, s(20));
        assert_eq!(w.check(s(500)), None, "still speaking");
        w.far_end(false, s(600));
        assert_eq!(w.check(s(629)), None);
        assert_eq!(w.check(s(630)), Some(CallEndChange::Offer));
    }

    #[test]
    fn call_end_the_offer_is_withdrawn_when_the_call_looks_alive_and_can_return() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        w.far_end(true, s(50));
        assert_eq!(w.check(s(50)), Some(CallEndChange::Withdraw));
        w.far_end(false, s(52));
        assert_eq!(w.check(s(81)), None);
        assert_eq!(w.check(s(82)), Some(CallEndChange::OfferAgain));
        // The session comes back.
        w.sessions(CallSessions::Active, s(90));
        assert_eq!(w.check(s(90)), Some(CallEndChange::Withdraw));
        assert_eq!(w.check(s(9_000)), None);
    }

    #[test]
    fn call_end_keep_recording_settles_it_until_a_session_returns_and_ends() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        w.keep_recording();
        for t in (44..5_000).step_by(2) {
            w.sessions(CallSessions::NoneActive, s(t));
            assert_eq!(w.check(s(t)), None, "at {t} s");
        }
        // Far-end sound and silence do not bring it back either.
        w.far_end(true, s(5_000));
        w.far_end(false, s(5_010));
        assert_eq!(w.check(s(6_000)), None);
        // A new session that ends does.
        w.sessions(CallSessions::Active, s(6_002));
        w.sessions(CallSessions::NoneActive, s(6_004));
        w.sessions(CallSessions::NoneActive, s(6_006));
        assert_eq!(w.check(s(6_035)), None);
        assert_eq!(w.check(s(6_036)), Some(CallEndChange::Offer));
    }

    #[test]
    fn call_end_a_cycle_asks_first_once_until_a_session_is_active_again() {
        let mut w = ended_at_12();
        assert_eq!(w.check(s(42)), Some(CallEndChange::Offer));
        // The far end comes and goes: asked again each time, but never as the first.
        let mut t = 50;
        for _ in 0..3 {
            w.far_end(true, s(t));
            assert_eq!(w.check(s(t)), Some(CallEndChange::Withdraw));
            w.far_end(false, s(t + 1));
            assert_eq!(w.check(s(t + 31)), Some(CallEndChange::OfferAgain));
            t += 40;
        }
        // A session active starts a new cycle.
        w.sessions(CallSessions::Active, s(t));
        assert_eq!(w.check(s(t)), Some(CallEndChange::Withdraw));
        w.sessions(CallSessions::NoneActive, s(t + 2));
        w.sessions(CallSessions::NoneActive, s(t + 4));
        assert_eq!(w.check(s(t + 34)), Some(CallEndChange::Offer));
    }

    #[test]
    fn call_end_keep_before_any_offer_changes_nothing_harmful() {
        let mut w = watch();
        w.keep_recording();
        assert_eq!(w.check(s(100)), None);
    }

    #[test]
    fn call_end_default_settings_are_the_documented_judgements() {
        let c = CallEndConfig::default();
        assert_eq!(c.polls_to_end, 2);
        assert_eq!(c.quiet_after, s(30));
        assert_eq!(c.stop_after, s(180));
        assert_eq!(c.stop_needs_reading_within, s(10));
    }
}
