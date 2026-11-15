//! Device presence (T11.3c; docs/sync-spec.md section 8, docs/sync.md section 10).
//!
//! Each device sends a sealed blob every [`PRESENCE_EVERY`]: its name, Earshot's version, its
//! kind, whether it writes summaries for the other devices, the writer it would use (phase 2)
//! and whether it is recording. The relay keeps the newest blob per device and answers with
//! its age by its own clock, so device clocks do not matter. A device is online when its
//! blob is at most [`ONLINE_WITHIN_S`] old and does not say gone ([`online`]).
//!
//! Presence is advisory: the blob's associated data holds no time, so the relay could hand an
//! old blob back as fresh. Nothing decides on presence alone (summary offload, T11.3d, also
//! needs the target's claim, a synced row the relay cannot forge).

use std::time::Duration;

use earshot_sync::wire::{B64, PresenceRequest};
use earshot_sync::{Keys, open_presence, seal_presence};
use serde::{Deserialize, Serialize};

use super::SyncProblem;

/// How often a device sends its presence.
pub const PRESENCE_EVERY: Duration = Duration::from_secs(60);
/// A device is online when its blob is at most this old (two missed beats, a judgement).
pub const ONLINE_WITHIN_S: u64 = 150;
/// The presence blob's format.
const PRESENCE_VERSION: u32 = 1;
/// The longest device name kept (characters).
pub const MAX_DEVICE_NAME: usize = 64;
/// Every presence plaintext is padded to this many bytes (the longest a v1 blob can be, a
/// 64-character name of 4-byte characters and a long version included, fits well inside).
pub const PADDED_LEN: usize = 640;

/// The presence blob's plaintext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    /// The format ([`PRESENCE_VERSION`]).
    pub v: u32,
    /// The device's name as its owner sees it.
    pub name: String,
    /// Earshot's version on it.
    pub app_version: String,
    /// `app` for Earshot on a computer (`server` for a summary worker, later).
    pub kind: String,
    /// Whether it writes summaries for the other devices ("Write summaries for my other
    /// devices").
    pub offers_summaries: bool,
    /// The kind of writer it would use ([`crate::offload::WriterKind::word`]), sent only while
    /// it offers summaries; `None` otherwise or for "no summary".
    pub writer_kind: Option<String>,
    /// Whether it is recording a meeting now.
    pub recording: bool,
    /// Whether it said it is going away (sync turned off, or removed).
    #[serde(default)]
    pub gone: bool,
    /// Filler that makes every blob [`PADDED_LEN`] bytes before sealing, so the relay cannot
    /// read a flag or the name's length from the blob's size.
    #[serde(default)]
    pub pad: String,
}

/// Whether a device whose blob is `age_s` old, and says `gone` or not, counts as online.
#[must_use]
pub const fn online(age_s: u64, gone: bool) -> bool {
    !gone && age_s <= ONLINE_WITHIN_S
}

/// What presence this device sends, decided without a clock (T11.3ca): a regular beat says
/// online, going away (the app quits or Windows sleeps) says gone once, and beats resume when
/// it comes back. Nothing is said before the first beat got through ("announced"), because the
/// other devices have not seen this one online yet.
///
/// The sync thread holds this behind one lock across every presence send, so sends never
/// overtake each other at the relay: a gone queued behind a beat in flight lands after it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Beats {
    announced: bool,
    away: bool,
}

impl Beats {
    /// Whether a regular beat should go out now (not while away).
    #[must_use]
    pub const fn beat_due(&self) -> bool {
        !self.away
    }

    /// An online beat got through.
    pub const fn sent_online(&mut self) {
        self.announced = true;
    }

    /// This device is going away. Returns whether to send gone now: once announced, and not
    /// twice for one going away.
    pub const fn go_away(&mut self) -> bool {
        let say = self.announced && !self.away;
        self.away = true;
        say
    }

    /// This device is back (Windows resumed): beats resume. Returns whether it had gone away.
    pub const fn come_back(&mut self) -> bool {
        let was = self.away;
        self.away = false;
        was
    }

    /// Whether the other devices have seen this one online.
    #[must_use]
    pub const fn announced(&self) -> bool {
        self.announced
    }
}

/// A device's name trimmed to [`MAX_DEVICE_NAME`] characters, control characters dropped;
/// `None` when nothing is left.
#[must_use]
pub fn clean_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_DEVICE_NAME)
        .collect();
    let cleaned = cleaned.trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// This computer's name as Windows knows it (`COMPUTERNAME`), else a fixed word.
#[must_use]
pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .and_then(|n| clean_name(&n))
        .unwrap_or_else(|| "Computer".to_owned())
}

/// The blob this device sends.
pub(crate) fn blob_of(
    presence: &Presence,
    keys: &Keys,
    device: earshot_core::identity::DeviceId,
) -> Result<Vec<u8>, SyncProblem> {
    let plain = padded(presence)?;
    seal_presence(keys, device, &plain).map_err(|_| SyncProblem::Internal)
}

/// `presence` as JSON of exactly [`PADDED_LEN`] bytes: its `pad` filled with `0` digits.
fn padded(presence: &Presence) -> Result<Vec<u8>, SyncProblem> {
    let mut p = presence.clone();
    p.pad.clear();
    let bare = serde_json::to_vec(&p)
        .map_err(|_| SyncProblem::Internal)?
        .len();
    let fill = PADDED_LEN.checked_sub(bare).ok_or(SyncProblem::Internal)?;
    p.pad = "0".repeat(fill);
    let plain = serde_json::to_vec(&p).map_err(|_| SyncProblem::Internal)?;
    debug_assert_eq!(plain.len(), PADDED_LEN);
    Ok(plain)
}

/// A device's presence opened from its blob; `None` when it cannot be opened or read (a
/// device of another vault, or a newer Earshot's format), shown without a name.
pub(crate) fn open_blob(
    keys: &Keys,
    device: earshot_core::identity::DeviceId,
    blob: &[u8],
) -> Option<Presence> {
    let plain = open_presence(keys, device, blob).ok()?;
    let p: Presence = serde_json::from_slice(&plain).ok()?;
    (p.v == PRESENCE_VERSION).then_some(Presence {
        name: clean_name(&p.name).unwrap_or_default(),
        ..p
    })
}

/// The presence this device sends now.
pub(crate) fn presence_now(
    name: String,
    offers_summaries: bool,
    writer_kind: Option<crate::offload::WriterKind>,
    recording: bool,
    gone: bool,
) -> Presence {
    Presence {
        v: PRESENCE_VERSION,
        name,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        kind: "app".to_owned(),
        offers_summaries,
        writer_kind: writer_kind.map(|k| k.word().to_owned()),
        recording,
        gone,
        pad: String::new(),
    }
}

/// `POST /v1/presence`.
pub(crate) fn request_of(blob: Vec<u8>) -> PresenceRequest {
    PresenceRequest { blob: B64(blob) }
}
