//! Failed registrations and slot takes per address (docs/sync-spec.md section 3): at most
//! [`MAX_FAILURES`] in [`WINDOW_S`], then `slow_down` until the oldest leaves the window.
//!
//! Behind a reverse proxy every request comes from the proxy's address, so the limit is in
//! effect shared by all callers: a guesser then also slows the owner for a minute, which is
//! acceptable for a call made once per device. `X-Forwarded-For` is not trusted.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

/// Failures allowed in [`WINDOW_S`].
pub(crate) const MAX_FAILURES: usize = 10;

/// The window, in seconds.
pub(crate) const WINDOW_S: u64 = 60;

#[derive(Default)]
pub(crate) struct Limiter(HashMap<IpAddr, VecDeque<u64>>);

impl Limiter {
    /// Drops failures older than the window.
    fn trim(&mut self, now: u64) {
        self.0.retain(|_, times| {
            while times.front().is_some_and(|t| t + WINDOW_S <= now) {
                times.pop_front();
            }
            !times.is_empty()
        });
    }

    /// Whether `ip` has used up its failures.
    pub(crate) fn blocked(&mut self, ip: IpAddr, now: u64) -> bool {
        self.trim(now);
        self.0.get(&ip).is_some_and(|t| t.len() >= MAX_FAILURES)
    }

    /// Records a failure of `ip`.
    pub(crate) fn record(&mut self, ip: IpAddr, now: u64) {
        self.trim(now);
        self.0.entry(ip).or_default().push_back(now);
    }
}
