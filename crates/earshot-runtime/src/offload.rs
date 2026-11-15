//! Summary offload (T11.3da; docs/sync.md section 10, docs/sync-spec.md section 10.1): one of
//! the user's devices asks another to write a meeting's summary, and the summary comes back as
//! an ordinary synced row.
//!
//! # The asker
//!
//! Where the summary step runs ([`crate::host::after_meeting`] with
//! [`crate::host::AfterMeeting::offload`]), [`choose_now`] decides once: when a helper is chosen
//! ([`set_chosen_helper`], device-local `settings/summary-offload-device`) and its presence says
//! it is online, writes summaries for the other devices, has a writer and is not recording, the
//! meeting gets a [`SummaryRequest`] ([`ask`]) and this device writes nothing now. Otherwise this
//! device's own writer runs and the outcome says why ([`OffloadChoice::HereInstead`], shown to
//! the user): a named choice, never a silent fallback.
//!
//! [`Offload::step`] (run by the sync thread after each tick) then withdraws a request that is
//! not claimed within [`CLAIM_WITHIN_MS`], or whose claiming helper's presence has been stale
//! for [`STALE_AFTER_S`], and hands the meeting back to the host to write here
//! ([`OffloadHost::write_here`]); a declined request is handed back the same way.
//!
//! # The helper
//!
//! [`Offload::step`] claims a request addressed to this device (or declines it when this device
//! no longer offers summaries), waits until it holds as many final segments as the request
//! counted (declining after [`TRANSCRIPT_WAIT_MS`]), then hands it to the host
//! ([`OffloadHost::write_for`]), which runs [`write_for_request`]: its own writer, style and
//! Dictionary, in its meeting queue behind its own work, through the ordinary summary path. The
//! request is read again in the summary's write transaction, and nothing is stored for one that
//! is no longer claimed ([`crate::summary::SummaryService::summarize_answering`]).
//!
//! # Privacy
//!
//! The transcript reaches whatever writer the helper uses: the asker's label names the writer
//! kind (presence [`WriterKind`]). A meeting kept on this computer is written only by a local
//! model on either device: the helper's host builds its writer with
//! [`SummaryPrivacy::KeptLocal`] and declines `kept_local_no_model` without one, and
//! [`crate::summary::SummaryService::summarize`] refuses any other service for it before the
//! transcript is read. Nothing here sends anything: the request is a synced row (sealed, to the
//! configured relay only) and presence was read by the sync client.
//!
//! # Who writes what
//!
//! Only the asker writes `Requested` and `Withdrawn`; only the target writes `Claimed`, `Done`
//! and `Declined`. A row written by any other device is ignored ([`authored`]). A withdrawal is
//! final ([`earshot_store::Store::apply_remote`]).

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use earshot_core::asr::CancelToken;
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::{
    MeetingRepo, RequestState, SummaryRequest, SummaryRequestRepo, TranscriptPass, live_segments,
};
use serde::{Deserialize, Serialize};

use crate::keep_local::SummaryPrivacy;
use crate::local::{self, DeviceRecord};
use crate::meeting_queue::{MeetingQueue, SummaryHold};
use crate::services::MeetingJob;
use crate::summary::SummaryService;
use crate::sync::{Peer, SyncClient, SyncVault};
use crate::{RuntimeError, Services};

/// `settings/summary-offload-device`: the device this one asks to write its summaries (a device
/// id, hyphenated; absent or empty: none).
pub(crate) const OFFLOAD_DEVICE_KEY: &str = "settings/summary-offload-device";

/// A request not claimed within this long (by this device's clock, from when it was asked) is
/// withdrawn and written here: one poll each way with room (90 s, a judgement).
pub const CLAIM_WITHIN_MS: u64 = 90_000;

/// A claimed request whose helper has sent no presence for this long (the relay's age) is
/// withdrawn and written here (5 minutes, a judgement).
pub const STALE_AFTER_S: u64 = 300;

/// How long a helper waits, after claiming, for the transcript the request counted before it
/// declines `no_transcript` (5 minutes, a judgement: the final transcript arrives in the
/// asker's next ticks, and a re-transcription there can change the count for good).
pub const TRANSCRIPT_WAIT_MS: u64 = 5 * 60_000;

/// The [`RuntimeError::Conflict`] text of a summary whose request is no longer claimed here.
pub const REQUEST_NOT_CLAIMED: &str = "the summary request is no longer claimed here";

/// Why a helper declined, as the fixed word in [`SummaryRequest::reason`].
pub mod reason {
    /// A meeting kept on its computer, and no local summary model here.
    pub const KEPT_LOCAL_NO_MODEL: &str = "kept_local_no_model";
    /// This device no longer writes summaries for the others.
    pub const NOT_OFFERED: &str = "not_offered";
    /// This device has no summary writer that can run ("no summary", or one that cannot start).
    pub const NO_WRITER: &str = "no_writer";
    /// The transcript the request counted did not arrive in time.
    pub const NO_TRANSCRIPT: &str = "no_transcript";
    /// The meeting was in use here.
    pub const BUSY: &str = "busy";
    /// The summary failed here.
    pub const FAILED: &str = "failed";
}

/// The kind of writer a helper would use, as its presence carries it (the asker's label names
/// it, so the user knows where the transcript goes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterKind {
    /// The local summary model on that device: nothing leaves it.
    Local,
    /// The Anthropic Messages API with that device's key.
    Anthropic,
    /// An OpenAI-compatible endpoint.
    OpenaiCompatible,
    /// Claude through that device's Claude Code.
    ClaudeCode,
    /// OpenAI through that device's Codex.
    Codex,
}

impl WriterKind {
    /// Every kind.
    pub const ALL: [Self; 5] = [
        Self::Local,
        Self::Anthropic,
        Self::OpenaiCompatible,
        Self::ClaudeCode,
        Self::Codex,
    ];

    /// The word presence carries.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Anthropic => "anthropic",
            Self::OpenaiCompatible => "openai_compatible",
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
        }
    }

    /// Parses [`WriterKind::word`]; `None` for a word this build does not know.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.word() == word)
    }

    /// The kind of this device's summary writer (`None` for no summary).
    ///
    /// # Errors
    /// Store errors.
    pub fn of_settings(store: &earshot_store::Store) -> Result<Option<Self>, RuntimeError> {
        use crate::summary_settings::{EndpointProvider, SummaryWriter, summary_settings};
        let (s, _) = summary_settings(store)?;
        Ok(match s.writer {
            SummaryWriter::OnThisComputer => Some(Self::Local),
            SummaryWriter::OwnEndpoint => Some(match s.provider {
                EndpointProvider::Anthropic => Self::Anthropic,
                EndpointProvider::OpenAiCompatible => Self::OpenaiCompatible,
            }),
            SummaryWriter::ClaudeCode => Some(Self::ClaudeCode),
            SummaryWriter::Codex => Some(Self::Codex),
            SummaryWriter::NoSummary => None,
        })
    }
}

/// The helper this device asks (`None`: none chosen, or a stored value that is not a device id,
/// which is no choice).
///
/// # Errors
/// Store errors.
pub fn chosen_helper(store: &earshot_store::Store) -> Result<Option<DeviceId>, RuntimeError> {
    let Some(bytes) = store.local_value(OFFLOAD_DEVICE_KEY)? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_value::<DeviceId>(serde_json::Value::String(text)).ok())
}

/// Chooses the helper this device asks; `None` asks none (this device's writer, as before).
/// A device-local setting: each device keeps its own.
///
/// # Errors
/// Store errors; [`RuntimeError::Internal`] for this device itself.
pub fn set_chosen_helper(
    services: &Services,
    device: Option<DeviceId>,
) -> Result<(), RuntimeError> {
    if device == Some(services.store().device()) {
        return Err(RuntimeError::Internal("a device cannot be its own helper"));
    }
    let text = device.map(|d| d.to_string()).unwrap_or_default();
    let mut db = services.store();
    local::set(
        &mut db,
        &DeviceRecord::SummaryOffloadDevice.key(),
        text.as_bytes(),
    )?;
    Ok(())
}

/// Why the chosen helper is not asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Sync is off or not set up on this device.
    SyncOff,
    /// The relay could not be asked who is online.
    Unreachable,
    /// Its presence is too old, says gone, or cannot be read.
    Offline,
    /// It does not write summaries for the other devices.
    NotOffering,
    /// It has no summary writer.
    NoWriter,
    /// It is recording a meeting.
    Recording,
    /// The meeting has no final transcript to count.
    NoFinalTranscript,
    /// The request could not be written here.
    NotAsked,
}

/// What the summary step does about the chosen helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OffloadChoice {
    /// No helper is chosen: this device's writer, as before.
    Here,
    /// A helper is chosen and not available now: this device's writer, and the user is told.
    HereInstead {
        /// The chosen helper.
        device: DeviceId,
        /// Its name, when its presence could be read.
        name: Option<String>,
        /// Why.
        why: Unavailable,
    },
    /// Ask the helper.
    Ask {
        /// The helper.
        device: DeviceId,
        /// Its name, from its presence.
        name: String,
        /// The kind of writer it uses.
        writer: WriterKind,
    },
}

/// The decision of the summary step: `chosen` from [`chosen_helper`], `peers` from the relay
/// (`None` when sync is off or the relay could not be asked: `unreachable` says which).
#[must_use]
pub fn decide(
    chosen: Option<DeviceId>,
    peers: Option<&[Peer]>,
    unreachable: bool,
) -> OffloadChoice {
    let Some(device) = chosen else {
        return OffloadChoice::Here;
    };
    let instead = |name: Option<String>, why| OffloadChoice::HereInstead { device, name, why };
    let Some(peers) = peers else {
        return instead(
            None,
            if unreachable {
                Unavailable::Unreachable
            } else {
                Unavailable::SyncOff
            },
        );
    };
    let Some(peer) = peers.iter().find(|p| p.device == device) else {
        return instead(None, Unavailable::Offline);
    };
    let Some(presence) = peer.presence.as_ref() else {
        return instead(None, Unavailable::Offline);
    };
    let name = Some(presence.name.clone()).filter(|n| !n.is_empty());
    if !peer.online {
        return instead(name, Unavailable::Offline);
    }
    if !presence.offers_summaries {
        return instead(name, Unavailable::NotOffering);
    }
    if presence.recording {
        return instead(name, Unavailable::Recording);
    }
    let Some(writer) = presence.writer_kind.as_deref().and_then(WriterKind::parse) else {
        return instead(name, Unavailable::NoWriter);
    };
    OffloadChoice::Ask {
        device,
        name: name.unwrap_or_default(),
        writer,
    }
}

/// [`decide`] with this device's stored choice and the relay's presence, asked now (one request
/// through the sync client, without the store lock): what a host passes as
/// [`crate::host::AfterMeeting::offload`]. No request is made when no helper is chosen.
#[must_use]
pub fn choose_now(services: &Services, vault: std::sync::Arc<dyn SyncVault>) -> OffloadChoice {
    let (chosen, sync_on) = {
        let db = services.store();
        let chosen = chosen_helper(&db).ok().flatten();
        let on = crate::sync::sync_settings(&db).is_ok_and(|s| s.on);
        (chosen, on)
    };
    if chosen.is_none() {
        return OffloadChoice::Here;
    }
    if !sync_on || !vault.has().unwrap_or(false) {
        return decide(chosen, None, false);
    }
    match SyncClient::new(services.clone(), vault).and_then(|c| c.peers()) {
        Ok(peers) => decide(chosen, Some(&peers), false),
        Err(_) => decide(chosen, None, true),
    }
}

/// How many live final segments `meeting` holds: what a request counts and a helper waits for.
fn final_segments<R: MeetingRepo + ?Sized>(
    repo: &R,
    meeting: EntityId,
) -> Result<u64, RuntimeError> {
    Ok(
        u64::try_from(live_segments(repo, meeting, TranscriptPass::Final)?.len())
            .unwrap_or(u64::MAX),
    )
}

/// Asks `target` to write `meeting`'s summary: a [`SummaryRequest`] counting the meeting's
/// final segments, written in one transaction with this device's own record of when it asked
/// ([`OFFLOAD_TIMES_KEY`]). Returns its id, or `None` when the meeting has no final transcript to
/// count (it is then written here, [`Unavailable::NoFinalTranscript`]). A meeting that already
/// waits for a request this device asked gets no second one: that request's id is returned
/// ([`waiting_for`] says for whom).
///
/// # Errors
/// [`crate::edit::EditError::MeetingNotFound`] for an unknown or deleted meeting; store errors.
pub fn ask(
    services: &Services,
    meeting: EntityId,
    target: DeviceId,
) -> Result<Option<EntityId>, RuntimeError> {
    let me = services.store().device();
    let now = services.clock.now_unix_ms();
    crate::edit::write(services, |tx, stamp| {
        crate::edit::live_meeting(&*tx, meeting)?;
        if let Some(open) = open_request(&*tx, me, meeting)? {
            return Ok(Some(open.id));
        }
        let count = final_segments(&*tx, meeting)?;
        if count == 0 {
            return Ok(None);
        }
        let request = SummaryRequest::new(meeting, target, me, count, stamp);
        let id = request.id;
        tx.put_summary_request(request)?;
        let mut times = read_times(tx.local_value(OFFLOAD_TIMES_KEY)?.as_deref());
        times.requests.insert(
            id.to_string(),
            Since {
                state: RequestState::Requested.name().to_owned(),
                since_ms: now,
                absent_since_ms: None,
            },
        );
        crate::local::set_tx(tx, &DeviceRecord::OffloadTimes.key(), &encode_times(&times))?;
        Ok(Some(id))
    })
}

/// The request this device asked for `meeting` that is still open (requested or claimed), newest
/// first.
fn open_request<R: SummaryRequestRepo + ?Sized>(
    repo: &R,
    me: DeviceId,
    meeting: EntityId,
) -> Result<Option<SummaryRequest>, RuntimeError> {
    Ok(repo.summary_requests()?.into_iter().rev().find(|r| {
        r.meeting_id == meeting
            && r.asking_device == me
            && !r.meta.deleted
            && r.state.is_open()
            && authored(r)
    }))
}

/// `offload-times`: this device's own clock readings for the open requests it asked or was
/// asked (when it asked, first saw a claim, claimed, or first found the helper's presence
/// missing). Every timer of offload runs on the device clock from these, never on HLC wall
/// time, which can run ahead of the clock by up to the apply window. Device-local, pruned to the
/// open requests at every step.
pub(crate) const OFFLOAD_TIMES_KEY: &str = "offload-times";

/// [`OFFLOAD_TIMES_KEY`]'s format version.
const TIMES_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Times {
    /// By request id (hyphenated).
    requests: std::collections::BTreeMap<String, Since>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Since {
    /// The state the reading belongs to ([`RequestState::name`]): a new state starts anew.
    state: String,
    /// When this device asked (requested), first saw the claim (asker) or claimed (helper).
    since_ms: u64,
    /// When this device first found the helper's presence missing or unreadable (asker).
    absent_since_ms: Option<u64>,
}

/// The stored readings; none when absent or unreadable (the timers then start now: a wait can
/// only grow).
fn read_times(bytes: Option<&[u8]>) -> Times {
    bytes
        .and_then(|b| {
            let (v, payload) = earshot_store::split_version(b)?;
            (v == TIMES_VERSION)
                .then(|| serde_json::from_slice(payload).ok())
                .flatten()
        })
        .unwrap_or_default()
}

fn encode_times(times: &Times) -> Vec<u8> {
    earshot_store::encode_versioned(TIMES_VERSION, times).expect("times serialise")
}

/// The helper `meeting` waits for: a request this device asked that is still open (requested or
/// claimed), newest first. What the meeting shows as "Waiting for <device>".
///
/// # Errors
/// Store errors.
pub fn waiting_for(
    store: &earshot_store::Store,
    me: DeviceId,
    meeting: EntityId,
) -> Result<Option<DeviceId>, RuntimeError> {
    Ok(open_request(store, me, meeting)?.map(|r| r.target_device))
}

/// Whether `request`'s last write came from the device allowed to write its state: the asker
/// for requested and withdrawn, the target for claimed, done and declined. A row written by any
/// other device is ignored, whatever it says.
#[must_use]
pub fn authored(request: &SummaryRequest) -> bool {
    let writer = request.meta.updated_at.node;
    match request.state {
        RequestState::Requested | RequestState::Withdrawn => writer == request.asking_device,
        RequestState::Claimed | RequestState::Done | RequestState::Declined => {
            writer == request.target_device
        }
    }
}

/// `request`, read inside the summary's write transaction: it must still be claimed by `me`
/// for `meeting` and authored so. Returns it to mark done.
///
/// # Errors
/// [`RuntimeError::Conflict`] with [`REQUEST_NOT_CLAIMED`] otherwise; store errors.
pub(crate) fn still_claimed<R: SummaryRequestRepo + ?Sized>(
    repo: &R,
    request: EntityId,
    meeting: EntityId,
    me: DeviceId,
) -> Result<SummaryRequest, RuntimeError> {
    match repo.summary_request(request)? {
        Some(r)
            if r.meeting_id == meeting
                && r.target_device == me
                && r.state == RequestState::Claimed
                && !r.meta.deleted
                && authored(&r) =>
        {
            Ok(r)
        }
        _ => Err(RuntimeError::Conflict(REQUEST_NOT_CLAIMED)),
    }
}

/// Moves `request` from `from` to `to` (with `reason` for a decline), re-read inside one
/// transaction; returns whether it moved (it no longer stood at `from`, or the meeting is
/// deleted: nothing written).
fn move_request(
    services: &Services,
    request: EntityId,
    from: RequestState,
    to: RequestState,
    reason: &str,
) -> Result<bool, RuntimeError> {
    crate::edit::write(services, |tx, stamp| {
        let Some(mut r) = tx.summary_request(request)? else {
            return Ok(false);
        };
        if r.state != from || r.meta.deleted || !authored(&r) {
            return Ok(false);
        }
        if tx.meeting(r.meeting_id)?.is_none_or(|m| m.meta.deleted) {
            return Ok(false);
        }
        r.state = to;
        r.reason = reason.to_owned();
        r.meta.touch(stamp.hlc, stamp.clock);
        tx.put_summary_request(r)?;
        Ok(true)
    })
}

/// Declines `request`, claimed by this device, with `word` ([`reason`]): for a host whose work for
/// it could not start or ended in a panic. Returns whether it moved (it was no longer claimed).
///
/// # Errors
/// Store errors.
pub fn decline(
    services: &Services,
    request: EntityId,
    word: &'static str,
) -> Result<bool, RuntimeError> {
    move_request(
        services,
        request,
        RequestState::Claimed,
        RequestState::Declined,
        word,
    )
}

/// Why the asker writes a meeting's summary itself after asking a helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HereReason {
    /// The helper did not claim the request within [`CLAIM_WITHIN_MS`].
    NotClaimed,
    /// The helper claimed it, then its presence went stale for [`STALE_AFTER_S`].
    HelperStale,
    /// The helper declined, with its reason word ([`reason`]).
    Declined(String),
    /// Withdrawn earlier (by an earlier run of this device) and no summary since.
    Withdrawn,
}

/// A meeting the asker writes itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteHere {
    /// The meeting.
    pub meeting: EntityId,
    /// The request it had asked with.
    pub request: EntityId,
    /// The helper that was asked.
    pub helper: DeviceId,
    /// Why.
    pub why: HereReason,
}

/// What the host does for [`Offload::step`]. Both calls must return quickly: the host runs the
/// work on a thread of its own.
pub trait OffloadHost: Send + Sync {
    /// Write `job.meeting`'s summary here, with this device's own writer (a re-summary job:
    /// [`crate::keep_local::summary_privacy`] still decides the writer). Returns whether the work
    /// started; a refusal (a job of the meeting runs, the meeting is in use) is handed again at the
    /// next step.
    fn write_here(&self, job: WriteHere) -> bool;
    /// Write the summary another device asked for: run [`write_for_request`] in this device's
    /// meeting queue.
    fn write_for(&self, request: SummaryRequest);
}

/// What one [`Offload::step`] did. Counts only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Requests this device claimed.
    pub claimed: usize,
    /// Requests this device declined.
    pub declined: usize,
    /// Requests handed to the host to write for another device.
    pub handed: usize,
    /// Requests this device withdrew.
    pub withdrawn: usize,
    /// Meetings handed back to the host to write here.
    pub written_here: usize,
}

/// The offload state of one running process: which requests were already handed to the host,
/// so each is handed once per process (a restart hands an unfinished one again).
#[derive(Debug, Default)]
pub struct Offload {
    handed: Mutex<BTreeSet<EntityId>>,
    /// Requests that need nothing more of this device (done, or declined or withdrawn with a
    /// summary since), so a step does not read their meeting again.
    settled: Mutex<BTreeSet<EntityId>>,
}

/// A declined or withdrawn request created more than this long ago (by its HLC wall time, which
/// only runs ahead of the clock, so a request never looks older than it is) is not looked at
/// again: its write here was handed long before (7 days, a judgement bounding each step's reads).
/// Open requests are always looked at: their own rules end them within minutes.
pub const LOOK_BACK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// What [`Offload::step`] reads of one request, under one store lock.
struct Seen {
    request: SummaryRequest,
    /// The meeting is live here.
    meeting_live: bool,
    /// A summary of the meeting was created after the request (by HLC), live or not: one
    /// replaced by an undo still counts, so the user's choice is never written over.
    summarised_since: bool,
    /// The meeting's live final segments here.
    final_segments: u64,
}

impl Offload {
    /// A new state (nothing handed yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn hand_once(&self, request: EntityId) -> bool {
        self.handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(request)
    }

    fn unhand(&self, request: EntityId) {
        self.handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&request);
    }

    fn settle(&self, request: EntityId) {
        self.settled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(request);
    }

    fn is_settled(&self, request: EntityId) -> bool {
        self.settled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&request)
    }

    /// One pass over the requests this device asked or was asked, by this device's clock and the
    /// relay's presence from `peers` (asked at most once, and only when this device waits on a
    /// claimed request; `None` when it could not be read, which withdraws nothing on presence
    /// and still withdraws an unclaimed request on time). Writes only the states this device may
    /// write; hands work to `host`.
    ///
    /// # Errors
    /// Store errors; what was done before stays done.
    pub fn step(
        &self,
        services: &Services,
        peers: &mut dyn FnMut() -> Option<Vec<Peer>>,
        host: &dyn OffloadHost,
    ) -> Result<StepReport, RuntimeError> {
        let (me, now) = (services.store().device(), services.clock.now_unix_ms());
        let (seen, offers, mut times) = {
            let db = services.store();
            let offers = crate::sync::sync_settings(&db)
                .map(|s| s.offers_summaries)
                .unwrap_or(false);
            let times = read_times(db.local_value(OFFLOAD_TIMES_KEY)?.as_deref());
            let mut seen = Vec::new();
            for r in db.summary_requests()? {
                let mine = r.asking_device == me;
                let wanted = if mine {
                    r.state != RequestState::Done
                } else {
                    r.target_device == me && r.state.is_open()
                };
                if r.meta.deleted
                    || !wanted
                    || !authored(&r)
                    || self.is_settled(r.id)
                    || (!r.state.is_open()
                        && r.meta.created_at.wall_ms.saturating_add(LOOK_BACK_MS) < now)
                {
                    continue;
                }
                let meeting = db.meeting(r.meeting_id)?;
                let meeting_live = meeting.is_some_and(|m| !m.meta.deleted);
                let summarised_since = meeting_live
                    && db
                        .summaries(r.meeting_id)?
                        .iter()
                        .any(|s| s.meta.created_at > r.meta.created_at);
                let final_segments = if meeting_live && r.target_device == me {
                    final_segments(&**db, r.meeting_id)?
                } else {
                    0
                };
                seen.push(Seen {
                    request: r,
                    meeting_live,
                    summarised_since,
                    final_segments,
                });
            }
            (seen, offers, times)
        };
        let before = times.clone();
        // Readings are kept for open requests only.
        times.requests.retain(|id, _| {
            seen.iter()
                .any(|s| s.request.state.is_open() && s.request.id.to_string() == *id)
        });
        let waits_on_claim = seen.iter().any(|s| {
            s.meeting_live
                && s.request.asking_device == me
                && s.request.state == RequestState::Claimed
        });
        let peers = if waits_on_claim { peers() } else { None };
        let mut report = StepReport::default();
        let mut outcome = Ok(());
        for s in seen {
            let r = &s.request;
            if !s.meeting_live {
                self.settle(r.id);
                continue;
            }
            let done = if r.asking_device == me {
                self.as_asker(
                    services,
                    &s,
                    now,
                    peers.as_deref(),
                    &mut times,
                    host,
                    &mut report,
                )
            } else {
                self.as_helper(services, &s, now, offers, &mut times, host, &mut report)
            };
            if let Err(e) = done {
                outcome = Err(e);
                break;
            }
        }
        if times != before {
            local::set(
                &mut services.store(),
                &DeviceRecord::OffloadTimes.key(),
                &encode_times(&times),
            )?;
        }
        outcome.map(|()| report)
    }

    /// This device's reading for `r` in its current state, started now when there is none.
    fn since(times: &mut Times, r: &SummaryRequest, now: u64) -> u64 {
        let entry = times
            .requests
            .entry(r.id.to_string())
            .or_insert_with(|| Since {
                state: r.state.name().to_owned(),
                since_ms: now,
                absent_since_ms: None,
            });
        if entry.state != r.state.name() {
            *entry = Since {
                state: r.state.name().to_owned(),
                since_ms: now,
                absent_since_ms: None,
            };
        }
        entry.since_ms
    }

    #[allow(clippy::too_many_arguments)] // The step's state, read once and passed down.
    fn as_asker(
        &self,
        services: &Services,
        s: &Seen,
        now: u64,
        peers: Option<&[Peer]>,
        times: &mut Times,
        host: &dyn OffloadHost,
        report: &mut StepReport,
    ) -> Result<(), RuntimeError> {
        let r = &s.request;
        let why = match r.state {
            RequestState::Requested => {
                if now.saturating_sub(Self::since(times, r, now)) < CLAIM_WITHIN_MS {
                    return Ok(());
                }
                if !move_request(
                    services,
                    r.id,
                    RequestState::Requested,
                    RequestState::Withdrawn,
                    "",
                )? {
                    return Ok(());
                }
                report.withdrawn += 1;
                HereReason::NotClaimed
            }
            RequestState::Claimed => {
                Self::since(times, r, now);
                let Some(peers) = peers else {
                    return Ok(());
                };
                let entry = times
                    .requests
                    .get_mut(&r.id.to_string())
                    .expect("since inserted it");
                let stale = match presence_of(peers, r.target_device) {
                    HelperPresence::Seen { stale } => {
                        entry.absent_since_ms = None;
                        stale
                    }
                    HelperPresence::Missing => {
                        let since = *entry.absent_since_ms.get_or_insert(now);
                        now.saturating_sub(since) >= STALE_AFTER_S * 1000
                    }
                };
                if !stale
                    || !move_request(
                        services,
                        r.id,
                        RequestState::Claimed,
                        RequestState::Withdrawn,
                        "",
                    )?
                {
                    return Ok(());
                }
                report.withdrawn += 1;
                HereReason::HelperStale
            }
            RequestState::Declined | RequestState::Withdrawn if s.summarised_since => {
                self.settle(r.id);
                return Ok(());
            }
            RequestState::Declined => HereReason::Declined(r.reason.clone()),
            RequestState::Withdrawn => HereReason::Withdrawn,
            RequestState::Done => {
                self.settle(r.id);
                return Ok(());
            }
        };
        if self.hand_once(r.id) {
            let started = host.write_here(WriteHere {
                meeting: r.meeting_id,
                request: r.id,
                helper: r.target_device,
                why,
            });
            if started {
                report.written_here += 1;
            } else {
                // Refused (a job of the meeting runs, the meeting is in use): the next step tries
                // again.
                self.unhand(r.id);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // The step's state, read once and passed down.
    fn as_helper(
        &self,
        services: &Services,
        s: &Seen,
        now: u64,
        offers: bool,
        times: &mut Times,
        host: &dyn OffloadHost,
        report: &mut StepReport,
    ) -> Result<(), RuntimeError> {
        let r = &s.request;
        let claimed_at = match r.state {
            RequestState::Requested if !offers => {
                if move_request(
                    services,
                    r.id,
                    RequestState::Requested,
                    RequestState::Declined,
                    reason::NOT_OFFERED,
                )? {
                    report.declined += 1;
                }
                return Ok(());
            }
            RequestState::Requested => {
                if !move_request(
                    services,
                    r.id,
                    RequestState::Requested,
                    RequestState::Claimed,
                    "",
                )? {
                    return Ok(());
                }
                report.claimed += 1;
                let mut claimed = r.clone();
                claimed.state = RequestState::Claimed;
                Self::since(times, &claimed, now)
            }
            RequestState::Claimed => Self::since(times, r, now),
            _ => return Ok(()),
        };
        if s.final_segments >= r.segment_count {
            if self.hand_once(r.id) {
                report.handed += 1;
                let mut claimed = r.clone();
                claimed.state = RequestState::Claimed;
                host.write_for(claimed);
            }
        } else if now.saturating_sub(claimed_at) >= TRANSCRIPT_WAIT_MS
            && move_request(
                services,
                r.id,
                RequestState::Claimed,
                RequestState::Declined,
                reason::NO_TRANSCRIPT,
            )?
        {
            report.declined += 1;
        }
        Ok(())
    }
}

/// What the relay says of a claimed request's helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperPresence {
    /// Its presence was read: `stale` when it is [`STALE_AFTER_S`] old or older (a "gone" blob
    /// counts from when it was sent, so a helper that went to sleep is stale five minutes later).
    Seen {
        /// At least [`STALE_AFTER_S`] old.
        stale: bool,
    },
    /// No presence, or one that cannot be read: the asker gives it [`STALE_AFTER_S`] from when it
    /// first found it missing, on its own clock.
    Missing,
}

/// The helper `device`'s presence among `peers`.
#[must_use]
pub fn presence_of(peers: &[Peer], device: DeviceId) -> HelperPresence {
    match peers.iter().find(|p| p.device == device) {
        Some(p) if p.presence.is_some() => HelperPresence::Seen {
            stale: p.age_s >= STALE_AFTER_S,
        },
        _ => HelperPresence::Missing,
    }
}

/// The writer a helper's host builds for one request, with the summary privacy of its meeting.
pub enum HelperWriter {
    /// The service to write with, and what it holds of this computer (the meeting queue's
    /// [`crate::meeting_queue::QueueTicket::set_summary`]).
    Write(Box<SummaryService>, SummaryHold),
    /// Decline the request with this [`reason`] word (a kept-local meeting without a local
    /// model is [`reason::KEPT_LOCAL_NO_MODEL`]).
    Decline(&'static str),
}

/// What [`write_for_request`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum HelperOutcome {
    /// The summary was stored and the request marked done.
    Done {
        /// The stored summary.
        summary: EntityId,
    },
    /// The request was declined with this word.
    Declined(&'static str),
    /// The request is no longer claimed here (withdrawn, or gone): nothing was stored.
    NotClaimed,
    /// Cancelled (the app is quitting): the request stays claimed and is handed again at the
    /// next start.
    Cancelled,
}

/// Writes the summary another device asked for, through this device's ordinary summary path:
/// a turn in `queue` behind this device's own work, a [`MeetingJob::Processing`] lease, the
/// writer `writer_for` builds for the meeting's privacy, and
/// [`SummaryService::summarize_answering`], which stores nothing unless the request is still
/// claimed here and marks it done with the summary. Any failure declines the request, so the
/// asker writes it; a cancel leaves it claimed.
pub fn write_for_request(
    services: &Services,
    queue: &MeetingQueue,
    request: &SummaryRequest,
    writer_for: &dyn Fn(SummaryPrivacy) -> HelperWriter,
    cancel: &CancelToken,
) -> HelperOutcome {
    let me = services.store().device();
    let meeting = request.meeting_id;
    let ticket = queue.enter();
    let claimed = || still_claimed(&**services.store(), request.id, meeting, me).is_ok();
    if !claimed() {
        return HelperOutcome::NotClaimed;
    }
    let decline = |word: &'static str| match move_request(
        services,
        request.id,
        RequestState::Claimed,
        RequestState::Declined,
        word,
    ) {
        Ok(true) => HelperOutcome::Declined(word),
        _ => HelperOutcome::NotClaimed,
    };
    let privacy = crate::keep_local::summary_privacy(&services.store(), meeting);
    let (service, hold) = match writer_for(privacy) {
        HelperWriter::Write(service, hold) => (service, hold),
        HelperWriter::Decline(word) => return decline(word),
    };
    ticket.set_summary(hold);
    if !ticket.wait_turn(cancel) {
        return HelperOutcome::Cancelled;
    }
    let Ok(_lease) = services.lease_meeting_alone(meeting, MeetingJob::Processing) else {
        return decline(reason::BUSY);
    };
    if !claimed() {
        return HelperOutcome::NotClaimed;
    }
    let mut rng = crate::os_seeded_rng();
    match service.summarize_answering(services, meeting, request.id, &mut *rng, cancel) {
        Ok(outcome) => HelperOutcome::Done {
            summary: outcome.summary_id,
        },
        Err(RuntimeError::Conflict(REQUEST_NOT_CLAIMED)) => HelperOutcome::NotClaimed,
        Err(RuntimeError::SummaryKeptLocal(_)) => decline(reason::KEPT_LOCAL_NO_MODEL),
        Err(_) if cancel.is_cancelled() => HelperOutcome::Cancelled,
        Err(_) => decline(reason::FAILED),
    }
}

#[cfg(test)]
#[path = "offload_tests.rs"]
mod tests;
