//! When to offer to start recording because a call seems to have started (T12.86).
//!
//! [`CallStartWatch`] takes one reading at a time of whether a dedicated known call app (Teams,
//! Zoom and the like, never a browser) has the microphone open, and says when to show or take back
//! the question "start recording?". It starts nothing itself: the host does, and only on the
//! user's press.
//!
//! Measured on a real Teams call (PLAN T12.86, 2026-11-23): while the call rang no Teams session
//! existed; on answering, Teams opened a microphone session at once and its output sessions 0.3 s
//! later; at hang-up both closed in the same reading. So the offer follows the microphone:
//! - **Offer** after [`CallStartConfig::polls_to_start`] readings in a row with a call app holding
//!   the microphone, while no meeting is recording.
//! - **Take back** when the microphone is closed for [`CallStartConfig::polls_to_end`] readings in a
//!   row (the call ended unanswered), or a meeting starts recording.
//! - **"Not now"**, a meeting started, or a meeting that was recording during the call: no new
//!   question for this call. The next question needs the microphone closed (for
//!   [`CallStartConfig::polls_to_end`] readings) and then opened again: a new call.
//! - A reading that failed ([`MicReading::Unknown`]) is no evidence either way: it ends a run of
//!   readings and changes nothing else.
//!
//! **Judgements, not measurements:** the two-reading rules (one stray reading says nothing, as
//! T6.15's two-poll rule). Not measured: a meeting lobby or pre-join screen (the question may come
//! there, before joining), a video played in a chat, Zoom.

use earshot_store::Store;

use crate::RuntimeError;
use crate::local::DeviceRecord;

/// The local-state key of the General page's "Offer to record when a call starts".
pub const OFFER_KEY: &str = "settings/call-start-offer";

/// Whether Earshot offers to start recording when a call starts. On unless the user turned it off
/// (the offer only asks; nothing records without the user's press). A stored value other than "on"
/// or "off" reads as on.
///
/// # Errors
/// Store errors.
pub fn offer_on(store: &Store) -> Result<bool, RuntimeError> {
    Ok(!matches!(
        crate::local::read_setting(store, OFFER_KEY, |t| crate::local::on_off(&t).ok_or(t))?,
        crate::local::Setting::Known(false)
    ))
}

/// Stores "offer to record when a call starts".
///
/// # Errors
/// Store errors.
pub fn set_offer_on(store: &mut Store, on: bool) -> Result<(), RuntimeError> {
    let value: &[u8] = if on { b"on" } else { b"off" };
    crate::local::set(store, &DeviceRecord::CallStartOffer.key(), value).map_err(RuntimeError::from)
}

/// One reading of the microphone sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicReading {
    /// A dedicated known call app has an active microphone session.
    CallApp,
    /// None has.
    NoCallApp,
    /// The sessions could not be read.
    Unknown,
}

/// The rule's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallStartConfig {
    /// Readings in a row with a call app holding the microphone before the question is asked
    /// (default 2: a judgement, so one stray reading says nothing).
    pub polls_to_start: u32,
    /// Readings in a row without one before the call counts as over (default 2, the same
    /// judgement).
    pub polls_to_end: u32,
}

impl Default for CallStartConfig {
    fn default() -> Self {
        Self {
            polls_to_start: 2,
            polls_to_end: 2,
        }
    }
}

/// What to do with the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStartChange {
    /// Ask whether to start recording.
    Offer,
    /// Take the question away (the call ended unanswered, or a meeting started).
    Withdraw,
}

/// The rule (module docs).
#[derive(Debug, Clone, Default)]
pub struct CallStartWatch {
    config: CallStartConfig,
    /// Readings in a row with a call app holding the microphone.
    open_run: u32,
    /// Readings in a row without one.
    closed_run: u32,
    /// The current call (microphone held, not yet counted over) was already answered: asked and
    /// declined, started, or recorded. No question until it ends.
    settled: bool,
    /// The question is open.
    offered: bool,
}

impl CallStartWatch {
    /// A watch with no call seen.
    #[must_use]
    pub fn new(config: CallStartConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// One reading; `recording` says whether a meeting is recording at this reading.
    pub fn reading(&mut self, reading: MicReading, recording: bool) -> Option<CallStartChange> {
        match reading {
            MicReading::CallApp => {
                self.open_run = self.open_run.saturating_add(1);
                self.closed_run = 0;
            }
            MicReading::NoCallApp => {
                self.closed_run = self.closed_run.saturating_add(1);
                self.open_run = 0;
                if self.closed_run >= self.config.polls_to_end {
                    // The call is over: the next one may ask again.
                    self.settled = false;
                }
            }
            MicReading::Unknown => {
                self.open_run = 0;
                self.closed_run = 0;
            }
        }
        if recording {
            // A meeting recording this call settles it: once it ends, no question for the rest
            // of the call.
            self.settled = true;
            return self.withdraw();
        }
        if self.offered {
            return if self.closed_run >= self.config.polls_to_end {
                self.withdraw()
            } else {
                None
            };
        }
        if !self.settled && self.open_run >= self.config.polls_to_start {
            self.offered = true;
            return Some(CallStartChange::Offer);
        }
        None
    }

    /// The user answered "Not now", or pressed Start recording: the question closes and this call
    /// asks no more.
    pub fn answered(&mut self) {
        self.offered = false;
        self.settled = true;
    }

    /// Whether the question is open.
    #[must_use]
    pub fn offered(&self) -> bool {
        self.offered
    }

    fn withdraw(&mut self) -> Option<CallStartChange> {
        std::mem::replace(&mut self.offered, false).then_some(CallStartChange::Withdraw)
    }
}

#[cfg(test)]
mod tests {
    use super::MicReading::{CallApp, NoCallApp, Unknown};
    use super::*;

    fn watch() -> CallStartWatch {
        CallStartWatch::new(CallStartConfig::default())
    }

    /// Feeds `readings` with no meeting recording and returns every change.
    fn feed(w: &mut CallStartWatch, readings: &[MicReading]) -> Vec<CallStartChange> {
        readings
            .iter()
            .filter_map(|r| w.reading(*r, false))
            .collect()
    }

    #[test]
    fn call_start_two_readings_of_a_call_app_mic_offer_once() {
        let mut w = watch();
        assert_eq!(w.reading(NoCallApp, false), None);
        assert_eq!(w.reading(CallApp, false), None);
        assert_eq!(w.reading(CallApp, false), Some(CallStartChange::Offer));
        assert!(w.offered());
        assert_eq!(feed(&mut w, &[CallApp; 20]), []);
    }

    #[test]
    fn call_start_one_stray_reading_never_offers() {
        let mut w = watch();
        assert_eq!(
            feed(&mut w, &[CallApp, NoCallApp, CallApp, NoCallApp, CallApp]),
            []
        );
        // A failed reading breaks a run too.
        assert_eq!(feed(&mut w, &[NoCallApp, CallApp, Unknown, CallApp]), []);
    }

    #[test]
    fn call_start_a_call_ending_unanswered_takes_the_question_back() {
        let mut w = watch();
        assert_eq!(feed(&mut w, &[CallApp, CallApp]), [CallStartChange::Offer]);
        // One reading without says nothing; two end the call.
        assert_eq!(feed(&mut w, &[NoCallApp]), []);
        assert_eq!(feed(&mut w, &[NoCallApp]), [CallStartChange::Withdraw]);
        assert!(!w.offered());
        // The next call asks again.
        assert_eq!(feed(&mut w, &[CallApp, CallApp]), [CallStartChange::Offer]);
    }

    #[test]
    fn call_start_not_now_asks_nothing_until_a_new_call() {
        let mut w = watch();
        feed(&mut w, &[CallApp, CallApp]);
        w.answered();
        assert!(!w.offered());
        assert_eq!(feed(&mut w, &[CallApp; 30]), []);
        // A blip of one reading is the same call.
        assert_eq!(feed(&mut w, &[NoCallApp, CallApp, CallApp]), []);
        // Closed for two readings, then opened: a new call.
        assert_eq!(
            feed(&mut w, &[NoCallApp, NoCallApp, CallApp, CallApp]),
            [CallStartChange::Offer]
        );
    }

    #[test]
    fn call_start_never_offers_while_a_meeting_records() {
        let mut w = watch();
        for _ in 0..5 {
            assert_eq!(w.reading(CallApp, true), None);
        }
        // The meeting stopped while the call goes on: no question for this call.
        assert_eq!(feed(&mut w, &[CallApp; 10]), []);
        // A new call after it asks.
        assert_eq!(
            feed(&mut w, &[NoCallApp, NoCallApp, CallApp, CallApp]),
            [CallStartChange::Offer]
        );
    }

    #[test]
    fn call_start_a_meeting_starting_takes_the_question_back() {
        let mut w = watch();
        feed(&mut w, &[CallApp, CallApp]);
        assert_eq!(w.reading(CallApp, true), Some(CallStartChange::Withdraw));
        assert_eq!(w.reading(CallApp, true), None);
        assert_eq!(feed(&mut w, &[CallApp; 5]), []);
    }

    #[test]
    fn call_start_a_failed_reading_keeps_an_open_question() {
        let mut w = watch();
        feed(&mut w, &[CallApp, CallApp]);
        assert_eq!(feed(&mut w, &[Unknown, Unknown, CallApp]), []);
        assert!(w.offered());
        // Unknown between two closed readings does not count them in a row.
        assert_eq!(feed(&mut w, &[NoCallApp, Unknown, NoCallApp]), []);
        assert!(w.offered());
    }

    #[test]
    fn call_start_offer_is_on_until_turned_off() {
        let dir = crate::testutil::TempDir::new("call-start");
        let (svc, _) = crate::testutil::services(&dir);
        let mut db = svc.store();
        assert!(offer_on(&db).unwrap());
        assert_eq!(
            db.local_value(OFFER_KEY).unwrap(),
            None,
            "reading writes nothing"
        );
        set_offer_on(&mut db, false).unwrap();
        assert!(!offer_on(&db).unwrap());
        set_offer_on(&mut db, true).unwrap();
        assert!(offer_on(&db).unwrap());
        db.set_local_value(OFFER_KEY, b"later").unwrap();
        assert!(offer_on(&db).unwrap(), "only `off` turns it off");
        assert_eq!(DeviceRecord::CallStartOffer.key().as_str(), OFFER_KEY);
    }

    #[test]
    fn call_start_default_settings_are_the_documented_judgements() {
        let c = CallStartConfig::default();
        assert_eq!((c.polls_to_start, c.polls_to_end), (2, 2));
    }
}
