//! Hybrid logical clock.
//!
//! S. S. Kulkarni, M. Demirbas, D. Madeppa, B. Avva, M. Leone, "Logical Physical Clocks
//! and Consistent Snapshots in Globally Distributed Databases" (2014), Figure 5:
//!
//! - local or send event: `l' = l; l = max(l', pt); c = (l == l') ? c + 1 : 0`;
//! - receive event `m`: `l' = l; l = max(l', l_m, pt)`; `c = max(c, c_m) + 1` if
//!   `l == l' == l_m`, `c + 1` if `l == l'`, `c_m + 1` if `l == l_m`, else `0`.
//!
//! Timestamps are totally ordered by `(wall_ms, logical, node)`. If the logical counter
//! would overflow, the wall component is advanced by 1 ms and the counter reset, which
//! keeps the ordering strict.

use serde::{Deserialize, Serialize};

use super::clock::Clock;
use super::id::DeviceId;

/// A hybrid logical clock reading. Ordered by `wall_ms`, then `logical`, then `node`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HlcTimestamp {
    /// Logical wall time: the largest physical time (Unix ms) seen so far.
    pub wall_ms: u64,
    /// Counter distinguishing events with the same `wall_ms`.
    pub logical: u32,
    /// Device that issued the timestamp (final tiebreak).
    pub node: DeviceId,
}

/// Per-device hybrid logical clock state.
#[derive(Debug, Clone)]
pub struct Hlc {
    node: DeviceId,
    wall_ms: u64,
    logical: u32,
}

impl Hlc {
    /// A fresh clock for device `node`.
    pub fn new(node: DeviceId) -> Self {
        Hlc {
            node,
            wall_ms: 0,
            logical: 0,
        }
    }

    /// A clock for device `node` that continues after `high_water`, the greatest timestamp
    /// already stored or seen (any node): every later reading is greater than it, even if
    /// the wall clock is now behind (restart after a clock step).
    pub fn resume(node: DeviceId, high_water: HlcTimestamp) -> Self {
        Hlc {
            node,
            wall_ms: high_water.wall_ms,
            logical: high_water.logical,
        }
    }

    /// The device this clock issues timestamps for.
    pub fn node(&self) -> DeviceId {
        self.node
    }

    /// Timestamp for a local event (row created or updated, or a message sent).
    pub fn now<C: Clock + ?Sized>(&mut self, clock: &C) -> HlcTimestamp {
        let prev = self.wall_ms;
        self.wall_ms = prev.max(clock.now_unix_ms());
        if self.wall_ms == prev {
            self.bump(self.logical);
        } else {
            self.logical = 0;
        }
        self.stamp()
    }

    /// Merges a timestamp received from another device and returns the timestamp of the
    /// receive event, which is greater than both `remote` and every earlier local one.
    pub fn observe<C: Clock + ?Sized>(&mut self, remote: HlcTimestamp, clock: &C) -> HlcTimestamp {
        let prev = self.wall_ms;
        self.wall_ms = prev.max(remote.wall_ms).max(clock.now_unix_ms());
        let (local, from_remote) = (self.wall_ms == prev, self.wall_ms == remote.wall_ms);
        match (local, from_remote) {
            (true, true) => self.bump(self.logical.max(remote.logical)),
            (true, false) => self.bump(self.logical),
            (false, true) => self.bump(remote.logical),
            (false, false) => self.logical = 0,
        }
        self.stamp()
    }

    /// Sets `logical = base + 1`, carrying into `wall_ms` on overflow.
    fn bump(&mut self, base: u32) {
        match base.checked_add(1) {
            Some(c) => self.logical = c,
            None => {
                self.wall_ms = self.wall_ms.saturating_add(1);
                self.logical = 0;
            }
        }
    }

    fn stamp(&self) -> HlcTimestamp {
        HlcTimestamp {
            wall_ms: self.wall_ms,
            logical: self.logical,
            node: self.node,
        }
    }
}
