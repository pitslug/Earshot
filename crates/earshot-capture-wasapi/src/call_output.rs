//! Which output a call plays on (T6.15): the rule that moves a device loopback onto the render
//! endpoint holding a known call app's session, so a call on a headset is recorded while Windows
//! defaults to the speakers. Only `LoopbackRequest::FollowCallOutput` uses it, and only with no
//! output chosen on the Audio page: the app's meetings still record the whole default output until
//! a real call has proven this (private notes). Platform independent; the capture loop gives
//! it readings and does the move.
//!
//! The rule, for each reading:
//! - A candidate is an output with a known call app's session that is active **and has sound**
//!   ([`CALL_OUTPUT_AUDIBLE`]). Teams holds active sessions that play nothing (T6.15a saw two on one
//!   process), and a silent one must not pull the recording away from where the sound is. Only the
//!   best-ranked app with sound counts ([`crate::session::KNOWN_CALL_APPS`]), so a browser playing on
//!   the default output does not hold the recording away from Teams on a headset.
//! - On a candidate already: stay. No candidate: stay, whatever the recording is on. It never moves
//!   because something stopped, only towards sound.
//! - Elsewhere: move only once the same output has been the target for [`POLLS_TO_MOVE`] readings in
//!   a row, so a passing sound does not move it. With several, the one already waited for, else the
//!   loudest.
//! - The capture loop opens the new stream before it closes the old one. An output it cannot open is
//!   [`refused`](CallOutputFollower::refused) for the rest of the capture and reported once, and the
//!   recording stays where it was: this can never leave a track without a stream.
//!
//! The polls and the audible level are judgements, not measurements. Before the first move the
//! recording is on the default output, so a call on another output loses up to about two poll
//! intervals at the start.

use std::collections::BTreeSet;

use crate::session::{SessionPeak, known_call_app};

/// Readings in a row before the recording moves: a judgement (PLAN T6.15's two-poll rule).
pub const POLLS_TO_MOVE: u32 = 2;

/// At or above this session meter peak (-60 dBFS) a session has sound: the level `teams_probe`
/// reads as audible. A judgement.
pub const CALL_OUTPUT_AUDIBLE: f32 = 1e-3;

/// The output to move to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallOutput {
    /// `IMMDevice::GetId`.
    pub id: String,
    /// Its friendly name (for messages; the id decides).
    pub name: String,
}

/// The rule's state over a capture (module docs).
#[derive(Clone, Debug, Default)]
pub struct CallOutputFollower {
    /// The output being waited for and how many readings in a row have named it.
    pending: Option<(String, u32)>,
    refused: BTreeSet<String>,
}

impl CallOutputFollower {
    /// A follower at the start of a capture.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One reading, with the recording on the output `current` (`None`: it has no id or no
    /// stream). `Some` when it should move now.
    pub fn reading(
        &mut self,
        current: Option<&str>,
        sessions: &[SessionPeak],
    ) -> Option<CallOutput> {
        let candidates = candidates(sessions);
        let current_is_one = current.is_some_and(|id| candidates.iter().any(|c| c.id == id));
        let open: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| !self.refused.contains(c.id))
            .collect();
        if current_is_one || open.is_empty() {
            self.pending = None;
            return None;
        }
        let waited_for = self.pending.as_ref().and_then(|(id, n)| {
            open.iter()
                .find(|c| c.id == id.as_str())
                .map(|c| (*c, n + 1))
        });
        let (target, polls) = waited_for.unwrap_or_else(|| {
            let loudest = open
                .iter()
                .max_by(|a, b| a.peak.total_cmp(&b.peak).then_with(|| b.id.cmp(a.id)))
                .copied();
            // `open` is not empty.
            (loudest.unwrap_or(open[0]), 1)
        });
        if polls >= POLLS_TO_MOVE {
            self.pending = None;
            return Some(CallOutput {
                id: target.id.to_owned(),
                name: target.name.to_owned(),
            });
        }
        self.pending = Some((target.id.to_owned(), polls));
        None
    }

    /// A reading failed: no evidence, so the wait starts over.
    pub fn no_reading(&mut self) {
        self.pending = None;
    }

    /// The output `id` could not be opened: never tried again in this capture.
    pub fn refused(&mut self, id: &str) {
        self.refused.insert(id.to_owned());
    }
}

/// An output with a call app's sound on it.
#[derive(Clone, Copy, Debug)]
struct Candidate<'a> {
    id: &'a str,
    name: &'a str,
    peak: f32,
}

/// The outputs of the best-ranked known call app with sound, one entry per output (its loudest
/// session).
fn candidates(sessions: &[SessionPeak]) -> Vec<Candidate<'_>> {
    let sounding: Vec<(usize, &SessionPeak, f32)> = sessions
        .iter()
        .filter(|s| s.pid != 0 && s.active && !s.endpoint_id.is_empty())
        .filter_map(|s| {
            let peak = s.peak.filter(|p| *p >= CALL_OUTPUT_AUDIBLE)?;
            let (rank, _) = known_call_app(&s.process_name)?;
            Some((rank, s, peak))
        })
        .collect();
    let Some(best) = sounding.iter().map(|(rank, ..)| *rank).min() else {
        return Vec::new();
    };
    let mut out: Vec<Candidate<'_>> = Vec::new();
    for (_, s, peak) in sounding.iter().filter(|(rank, ..)| *rank == best) {
        match out.iter_mut().find(|c| c.id == s.endpoint_id) {
            Some(c) => c.peak = c.peak.max(*peak),
            None => out.push(Candidate {
                id: &s.endpoint_id,
                name: &s.endpoint,
                peak: *peak,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEAKERS: &str = "{speakers}";
    const HEADSET: &str = "{headset}";

    fn session(id: &str, process: &str, active: bool, peak: f32) -> SessionPeak {
        SessionPeak {
            endpoint: format!("name of {id}"),
            endpoint_id: id.to_owned(),
            pid: 100,
            process_name: process.to_owned(),
            active,
            peak: Some(peak),
        }
    }

    fn teams(id: &str, peak: f32) -> SessionPeak {
        session(id, "ms-teams.exe", true, peak)
    }

    #[test]
    fn call_output_moves_on_the_second_reading_in_a_row() {
        let mut f = CallOutputFollower::new();
        let call = [teams(HEADSET, 0.2)];
        assert_eq!(f.reading(Some(SPEAKERS), &call), None);
        assert_eq!(
            f.reading(Some(SPEAKERS), &call),
            Some(CallOutput {
                id: HEADSET.into(),
                name: "name of {headset}".into(),
            })
        );
    }

    #[test]
    fn call_output_a_passing_sound_does_not_move_it() {
        let mut f = CallOutputFollower::new();
        // One reading, then quiet, again and again: never two in a row.
        for _ in 0..20 {
            assert_eq!(f.reading(Some(SPEAKERS), &[teams(HEADSET, 0.2)]), None);
            assert_eq!(f.reading(Some(SPEAKERS), &[]), None);
        }
    }

    #[test]
    fn call_output_stays_when_the_call_is_on_the_output_recorded() {
        let mut f = CallOutputFollower::new();
        let both = [teams(SPEAKERS, 0.01), teams(HEADSET, 0.5)];
        for _ in 0..5 {
            assert_eq!(f.reading(Some(SPEAKERS), &both), None);
        }
    }

    #[test]
    fn call_output_a_silent_or_inactive_session_never_pulls_it_away() {
        let mut f = CallOutputFollower::new();
        let silent = [teams(HEADSET, 0.0)];
        let below = [teams(HEADSET, CALL_OUTPUT_AUDIBLE / 2.0)];
        let inactive = [session(HEADSET, "ms-teams.exe", false, 0.5)];
        let unread = [SessionPeak {
            peak: None,
            ..teams(HEADSET, 0.0)
        }];
        for sessions in [&silent, &below, &inactive, &unread] {
            for _ in 0..5 {
                assert_eq!(f.reading(Some(SPEAKERS), sessions), None);
            }
        }
    }

    #[test]
    fn call_output_only_known_call_apps_move_it() {
        let mut f = CallOutputFollower::new();
        let music = [session(HEADSET, "spotify.exe", true, 0.9)];
        for _ in 0..5 {
            assert_eq!(f.reading(Some(SPEAKERS), &music), None);
        }
    }

    #[test]
    fn call_output_a_dedicated_call_app_beats_a_browser_on_the_output_recorded() {
        let mut f = CallOutputFollower::new();
        let sessions = [
            session(SPEAKERS, "chrome.exe", true, 0.9),
            teams(HEADSET, 0.1),
        ];
        assert_eq!(f.reading(Some(SPEAKERS), &sessions), None);
        assert_eq!(
            f.reading(Some(SPEAKERS), &sessions).map(|o| o.id),
            Some(HEADSET.to_owned())
        );
    }

    #[test]
    fn call_output_nothing_on_any_output_leaves_it_where_it_is() {
        let mut f = CallOutputFollower::new();
        assert_eq!(f.reading(Some(HEADSET), &[]), None);
        assert_eq!(f.reading(None, &[]), None);
        // The call ended: the recording does not chase anything.
        let other = [session(SPEAKERS, "ms-teams.exe", true, 0.0)];
        assert_eq!(f.reading(Some(HEADSET), &other), None);
    }

    #[test]
    fn call_output_keeps_waiting_for_the_same_output_when_another_gets_louder() {
        let mut f = CallOutputFollower::new();
        const THIRD: &str = "{third}";
        assert_eq!(
            f.reading(Some(SPEAKERS), &[teams(HEADSET, 0.5), teams(THIRD, 0.1)]),
            None
        );
        let moved = f.reading(Some(SPEAKERS), &[teams(HEADSET, 0.1), teams(THIRD, 0.9)]);
        assert_eq!(moved.map(|o| o.id), Some(HEADSET.to_owned()));
    }

    #[test]
    fn call_output_a_moved_target_starts_the_wait_over() {
        let mut f = CallOutputFollower::new();
        const THIRD: &str = "{third}";
        assert_eq!(f.reading(Some(SPEAKERS), &[teams(HEADSET, 0.5)]), None);
        assert_eq!(f.reading(Some(SPEAKERS), &[teams(THIRD, 0.5)]), None);
        assert_eq!(
            f.reading(Some(SPEAKERS), &[teams(THIRD, 0.5)])
                .map(|o| o.id),
            Some(THIRD.to_owned())
        );
    }

    #[test]
    fn call_output_a_failed_reading_starts_the_wait_over() {
        let mut f = CallOutputFollower::new();
        assert_eq!(f.reading(Some(SPEAKERS), &[teams(HEADSET, 0.5)]), None);
        f.no_reading();
        assert_eq!(f.reading(Some(SPEAKERS), &[teams(HEADSET, 0.5)]), None);
        assert!(f.reading(Some(SPEAKERS), &[teams(HEADSET, 0.5)]).is_some());
    }

    #[test]
    fn call_output_a_refused_output_is_never_tried_again() {
        let mut f = CallOutputFollower::new();
        let call = [teams(HEADSET, 0.5)];
        assert_eq!(f.reading(Some(SPEAKERS), &call), None);
        assert!(f.reading(Some(SPEAKERS), &call).is_some());
        f.refused(HEADSET);
        for _ in 0..10 {
            assert_eq!(f.reading(Some(SPEAKERS), &call), None);
        }
    }

    #[test]
    fn call_output_a_browser_does_not_take_over_when_the_call_output_was_refused() {
        // The call is on the refused output: recording what a browser plays elsewhere is no gain.
        let mut f = CallOutputFollower::new();
        f.refused(HEADSET);
        const THIRD: &str = "{third}";
        let sessions = [teams(HEADSET, 0.5), session(THIRD, "chrome.exe", true, 0.9)];
        for _ in 0..5 {
            assert_eq!(f.reading(Some(SPEAKERS), &sessions), None);
        }
    }

    #[test]
    fn call_output_a_session_without_an_endpoint_id_cannot_be_followed() {
        let mut f = CallOutputFollower::new();
        let call = [teams("", 0.5)];
        for _ in 0..5 {
            assert_eq!(f.reading(Some(SPEAKERS), &call), None);
        }
    }

    #[test]
    fn call_output_settings_are_the_documented_judgements() {
        assert_eq!(POLLS_TO_MOVE, 2);
        assert!((CALL_OUTPUT_AUDIBLE - 1e-3).abs() < f32::EPSILON);
    }
}
