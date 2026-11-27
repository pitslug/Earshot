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
//! for [`STALE_AFTER_S`], and hands the meeting back to the host ([`OffloadHost::continue_run`]);
//! a declined request is handed back the same way. Since T12.63a the meeting's run of the order
//! of summary writers decides what a hand-back does ([`crate::summary_order::run`]): the same run
//! goes on from its copied steps, a named run ends, a request with no run record is written here
//! as before ([`Continuation::Legacy`]), and an unreadable record or one naming another request is
//! settled with nothing written or sent. A `Done` request a waiting run names ends that run.
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

    /// The kind of this device's summary writer: the first writer step of its order of summary
    /// writers (T12.63a, [`crate::summary_order`]), which is the one writer it runs for another
    /// device's request ([`crate::summary_order::run::helper_writer`]); `None` without one.
    ///
    /// # Errors
    /// Store errors.
    pub fn of_order(store: &earshot_store::Store) -> Result<Option<Self>, RuntimeError> {
        let (order, _) = crate::summary_order::summary_order(store)?;
        let (settings, _) = crate::summary_settings::summary_settings(store)?;
        Ok(order
            .first_writer()
            .map(|w| crate::summary_order::run::kind_of(w, &settings)))
    }

    /// [`WriterKind::of_order`] (the name before T12.63a).
    ///
    /// # Errors
    /// Store errors.
    pub fn of_settings(store: &earshot_store::Store) -> Result<Option<Self>, RuntimeError> {
        Self::of_order(store)
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
/// A device-local setting: each device keeps its own. Stored as the order's step-1 computer
/// ([`crate::summary_order::SummaryOrder::with_helper`]) through
/// [`crate::summary_order::set_summary_order`], the one writer of
/// `settings/summary-offload-device` (T12.63a).
///
/// # Errors
/// Store errors; [`RuntimeError::Internal`] for this device itself.
pub fn set_chosen_helper(
    services: &Services,
    device: Option<DeviceId>,
) -> Result<(), RuntimeError> {
    use crate::summary_order::{
        SummaryOrderError, set_summary_order_keeping_writer, summary_order,
    };
    let mut db = services.store();
    if device == Some(db.device()) {
        return Err(RuntimeError::Internal("a device cannot be its own helper"));
    }
    let (order, _) = summary_order(&db)?;
    let order = order.with_helper(device);
    // A stored writer word that does not read as itself is kept, never replaced by a default.
    set_summary_order_keeping_writer(&mut db, &order).map_err(|e| match e {
        SummaryOrderError::Store(e) => e,
        _ => RuntimeError::Internal("the order of summary writers could not be kept"),
    })?;
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

/// [`decide`] for `device` (a computer step of the order, T12.63a) with the relay's presence,
/// asked now (one request through the sync client, without the store lock).
#[must_use]
pub fn choose_device_now(
    services: &Services,
    vault: std::sync::Arc<dyn SyncVault>,
    device: DeviceId,
) -> OffloadChoice {
    let sync_on = crate::sync::sync_settings(&services.store()).is_ok_and(|s| s.on);
    if !sync_on || !vault.has().unwrap_or(false) {
        return decide(Some(device), None, false);
    }
    match SyncClient::new(services.clone(), vault).and_then(|c| c.peers()) {
        Ok(peers) => decide(Some(device), Some(&peers), false),
        Err(_) => decide(Some(device), None, true),
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
    Ok(ask_for(services, meeting, target)?.map(|a| a.request))
}

/// A request [`ask_for`] answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asked {
    /// The request.
    pub request: EntityId,
    /// The device it asks: `target`, or the open request's own when the meeting already waited.
    pub target: DeviceId,
}

/// [`ask`], saying which device the request it answers with asks: a meeting that already waited
/// for another device keeps that request, and its caller must name that device, not `target`.
///
/// # Errors
/// As [`ask`].
pub fn ask_for(
    services: &Services,
    meeting: EntityId,
    target: DeviceId,
) -> Result<Option<Asked>, RuntimeError> {
    ask_inner(services, meeting, target, None)
}

/// [`ask_for`] as a computer step of a run of the order (T12.63a): in the transaction that writes
/// the request, the run's record (`waiting`, re-read and still of the same run) names it in
/// `waiting_on`, so no open request exists without a record naming it. A record of another run
/// writes nothing.
///
/// # Errors
/// As [`ask_for`]; [`RuntimeError::Conflict`] when the record is no longer of `waiting`'s run.
pub fn ask_for_run(
    services: &Services,
    meeting: EntityId,
    target: DeviceId,
    waiting: &crate::summary_order::run::SummaryRun,
) -> Result<Option<Asked>, RuntimeError> {
    ask_inner(services, meeting, target, Some(waiting))
}

fn ask_inner(
    services: &Services,
    meeting: EntityId,
    target: DeviceId,
    run: Option<&crate::summary_order::run::SummaryRun>,
) -> Result<Option<Asked>, RuntimeError> {
    let me = services.store().device();
    let now = services.clock.now_unix_ms();
    let name_it = |tx: &mut earshot_store::StoreTx<'_>, asked: Asked| -> Result<(), RuntimeError> {
        let Some(run) = run else {
            return Ok(());
        };
        #[cfg(test)]
        if let Some(e) = crate::summary_order::run::fault::take_ask() {
            return Err(e);
        }
        let mut waiting = run.clone();
        waiting.waiting_on = Some(asked.request);
        if crate::summary_order::run::put_own_tx(tx, meeting, &waiting)? {
            Ok(())
        } else {
            Err(RuntimeError::Conflict("the summary run was replaced"))
        }
    };
    crate::edit::write(services, |tx, stamp| {
        crate::edit::live_meeting(&*tx, meeting)?;
        if let Some(open) = open_request(&*tx, me, meeting)? {
            let asked = Asked {
                request: open.id,
                target: open.target_device,
            };
            name_it(tx, asked)?;
            return Ok(Some(asked));
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
        let asked = Asked {
            request: id,
            target,
        };
        name_it(tx, asked)?;
        Ok(Some(asked))
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

/// `offload-times`: this device's own clock readings. For the open requests it asked or was
/// asked: when it asked, first saw a claim, claimed, or first found the helper's presence
/// missing. For the declined or withdrawn requests it asked: when it first saw them closed, when
/// their write here started, and when a summary written since settled them; a closed request is
/// aged out ([`LOOK_BACK_MS`]) only from its start or its settling, so one whose write here never
/// started is never left. Every timer of offload runs on the device clock from these, never on
/// HLC wall time, which can run ahead of the clock by up to the apply window. Device-local:
/// open readings are pruned to the open requests at every step, closed ones to the requests the
/// store still holds, and a meeting's go with its deletion ([`forget_meeting_times`]). A step's
/// write re-reads and merges, so an `ask` meanwhile is never lost.
pub(crate) const OFFLOAD_TIMES_KEY: &str = "offload-times";

/// [`OFFLOAD_TIMES_KEY`]'s format version.
const TIMES_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Times {
    /// Open requests, by request id (hyphenated).
    requests: std::collections::BTreeMap<String, Since>,
    /// Declined or withdrawn requests this device asked, by request id (hyphenated).
    #[serde(default)]
    closed: std::collections::BTreeMap<String, Closed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Closed {
    /// When this device first saw it closed.
    seen_ms: u64,
    /// When its write here was first handed and started (a later start, after a restart, keeps
    /// it, so the look-back runs from the first).
    started_ms: Option<u64>,
    /// When a summary written since it was asked settled it.
    settled_ms: Option<u64>,
}

impl Closed {
    /// Whether it is left alone: it started or settled more than [`LOOK_BACK_MS`] ago.
    fn aged(&self, now: u64) -> bool {
        self.started_ms
            .or(self.settled_ms)
            .is_some_and(|at| now.saturating_sub(at) > LOOK_BACK_MS)
    }
}

/// `stored` with what one step changed (`before` to `after`) applied: a value the step set
/// overwrites, a value the step removed goes only if it is still what the step read, and
/// anything written meanwhile (an `ask`'s new reading) stays.
fn merge<V: PartialEq + Clone>(
    stored: &mut std::collections::BTreeMap<String, V>,
    before: &std::collections::BTreeMap<String, V>,
    after: &std::collections::BTreeMap<String, V>,
) {
    for (k, v) in after {
        if before.get(k) != Some(v) {
            stored.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in before {
        if !after.contains_key(k) && stored.get(k) == Some(v) {
            stored.remove(k);
        }
    }
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

/// Writes what one step changed (`before` to `after`) over the stored readings ([`merge`]), and
/// drops every reading of a request whose meeting is deleted or gone: a step that read the
/// readings before the meeting's deletion forgot them ([`forget_meeting_times`]) never writes
/// them back (T12.70).
///
/// # Errors
/// Store errors.
fn store_step_times(
    tx: &mut earshot_store::StoreTx<'_>,
    before: &Times,
    after: &Times,
) -> Result<(), RuntimeError> {
    let mut stored = read_times(tx.local_value(OFFLOAD_TIMES_KEY)?.as_deref());
    merge(&mut stored.requests, &before.requests, &after.requests);
    merge(&mut stored.closed, &before.closed, &after.closed);
    let mut gone = BTreeSet::new();
    for r in tx.summary_requests()? {
        if tx.meeting(r.meeting_id)?.is_none_or(|m| m.meta.deleted) {
            gone.insert(r.id.to_string());
        }
    }
    stored.requests.retain(|id, _| !gone.contains(id));
    stored.closed.retain(|id, _| !gone.contains(id));
    local::set_tx(
        tx,
        &DeviceRecord::OffloadTimes.key(),
        &encode_times(&stored),
    )?;
    Ok(())
}

/// Drops the readings of `meeting`'s requests, open and closed (T12.70): called by
/// [`crate::Services::delete_meeting`], the one deletion path (in the app and for a pulled tombstone).
/// Leaves the record as it is when it holds none of them.
///
/// # Errors
/// Store errors.
pub(crate) fn forget_meeting_times(
    tx: &mut earshot_store::StoreTx<'_>,
    meeting: EntityId,
) -> Result<(), RuntimeError> {
    let Some(bytes) = tx.local_value(OFFLOAD_TIMES_KEY)? else {
        return Ok(());
    };
    let mut times = read_times(Some(&bytes));
    let before = times.clone();
    for r in tx.summary_requests()? {
        if r.meeting_id == meeting {
            let id = r.id.to_string();
            times.requests.remove(&id);
            times.closed.remove(&id);
        }
    }
    if times != before {
        local::set_tx(tx, &DeviceRecord::OffloadTimes.key(), &encode_times(&times))?;
    }
    Ok(())
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

/// The meetings that wait for one of `devices`: those with a request this device asked of it
/// that is still open (requested or claimed). For a host that has just heard such a device's
/// name: the window reads these meetings again, so "Waiting for <device>" names it (T14.50).
///
/// # Errors
/// Store errors.
pub fn meetings_waiting_on(
    store: &earshot_store::Store,
    me: DeviceId,
    devices: &[DeviceId],
) -> Result<BTreeSet<EntityId>, RuntimeError> {
    Ok(store
        .summary_requests()?
        .into_iter()
        .filter(|r| {
            r.asking_device == me
                && !r.meta.deleted
                && r.state.is_open()
                && authored(r)
                && devices.contains(&r.target_device)
        })
        .map(|r| r.meeting_id)
        .collect())
}

/// The meetings whose summary this device waits for on another computer, as
/// [`waiting_shown`] says it meeting by meeting, for the meeting list (T14.66): one read of every
/// request, then one read of the summaries of each meeting found (usually none or one), through
/// guarded readers, so a damaged row never fails the list. A meeting whose request or summary rows
/// include one that does not read is left out ("no meeting waits"), and a store error gives an
/// empty set. Returns the set and how many damaged rows were skipped (for the damage scans).
#[must_use]
pub fn meetings_waiting_shown(
    read: &earshot_store::GuardedRead<'_>,
    me: DeviceId,
) -> (BTreeSet<EntityId>, usize) {
    let Ok(requests) = read.summary_requests_or_damaged() else {
        return (BTreeSet::new(), read.damaged_skipped());
    };
    // Each meeting's newest open request this device asked, as `open_request` picks it.
    let mut newest: std::collections::BTreeMap<EntityId, SummaryRequest> =
        std::collections::BTreeMap::new();
    for r in requests {
        if r.asking_device == me && !r.meta.deleted && r.state.is_open() && authored(&r) {
            newest.insert(r.meeting_id, r);
        }
    }
    let mut shown = BTreeSet::new();
    for (meeting, request) in newest {
        let before = read.damaged_skipped();
        let Ok(summaries) = read.summaries_or_damaged(meeting) else {
            continue;
        };
        if read.damaged_skipped() > before {
            continue;
        }
        if !summaries
            .iter()
            .any(|s| s.meta.created_at > request.meta.created_at)
        {
            shown.insert(meeting);
        }
    }
    (shown, read.damaged_skipped())
}

/// [`waiting_for`] as the meeting's page shows it (T12.81): `None` also once the meeting holds a
/// summary created after the open request ([`summarised_since`]), which can arrive before the
/// request's own row does
/// (the helper writes both in one transaction; sync may bring them in different pages).
///
/// # Errors
/// Store errors.
pub fn waiting_shown(
    store: &earshot_store::Store,
    me: DeviceId,
    meeting: EntityId,
) -> Result<Option<DeviceId>, RuntimeError> {
    let Some(r) = open_request(store, me, meeting)? else {
        return Ok(None);
    };
    Ok((!summarised_since(store, &r)?).then_some(r.target_device))
}

/// Whether `request`'s meeting holds a summary created after the request: the one rule the
/// offload step (a closed request settles) and the page's banner (the waiting ends) share. A
/// summary row counts whether or not it was later tombstoned (replaced by a re-summary or the
/// convergence rules, undone): a summary was written after the request either way.
///
/// # Errors
/// Store errors.
pub fn summarised_since<R: MeetingRepo + ?Sized>(
    repo: &R,
    request: &SummaryRequest,
) -> Result<bool, RuntimeError> {
    Ok(repo
        .summaries(request.meeting_id)?
        .iter()
        .any(|s| s.meta.created_at > request.meta.created_at))
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
    /// How the meeting's run of the order goes on (T12.63a).
    pub continuation: Continuation,
}

/// How a run goes on after a computer handed its request back (docs/summary-order.md 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuation {
    /// The meeting has no run record (a request asked before T12.63a): write it here as before,
    /// with the order's first writer step under the meeting's privacy
    /// ([`crate::summary_order::run::RunStart::Legacy`]).
    Legacy,
    /// The record waits on this request in an order run: a re-summarise job continues the same
    /// run from its copied steps ([`crate::summary_order::run::RunStart::Continue`] with this
    /// word).
    Order {
        /// The computer's outcome.
        word: crate::summary_order::run::OutcomeWord,
    },
}

/// What the host answers [`OffloadHost::continue_run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueAnswer {
    /// The work started.
    Started,
    /// The meeting is busy (a job of the meeting runs, the meeting is in use): handed again at the
    /// next step.
    Refused,
    /// Nothing to do: the request is settled and not read again in this process.
    Settled,
}

/// What the host does for [`Offload::step`]. Every call must return quickly: the host runs the
/// work on a thread of its own.
pub trait OffloadHost: Send + Sync {
    /// Go on with `job.meeting`'s summary after the computer handed its request back, as
    /// `job.continuation` says (a re-summary job; [`crate::keep_local::summary_privacy`] still
    /// decides the writers). A refusal is handed again at the next step.
    fn continue_run(&self, job: WriteHere) -> ContinueAnswer;
    /// Write the summary another device asked for: run [`write_for_request`] in this device's
    /// meeting queue, with the token of a [`Writing`] entry (T14.66). A request already being
    /// written (a second hand, by a new sync thread) starts nothing.
    fn write_for(&self, request: SummaryRequest);
    /// The requests being written now for other devices ([`Writing::ids`]).
    fn writing(&self) -> Vec<EntityId>;
    /// Stop writing `request`: its asker withdrew it ([`Writing::cancel`]).
    fn cancel_writing(&self, request: EntityId);
}

/// The summaries a host is writing for other devices, each with its write's cancel token
/// (T14.66, docs/summary-order.md 11.3). Owned by the host, which outlives every sync thread, so a
/// thread replaced mid-write leaves the write's token here: the next thread's hand of the same
/// request starts nothing, and its step cancels the running write when the withdrawal arrives.
///
/// Its lock is a leaf: nothing else is locked under it, and nothing is called under it.
#[derive(Debug, Default)]
pub struct Writing {
    state: Mutex<WritingState>,
}

#[derive(Debug, Default)]
struct WritingState {
    tokens: std::collections::BTreeMap<EntityId, CancelToken>,
    /// Set by [`Writing::cancel_all`]: no new write starts.
    stopping: bool,
}

/// One write's place in a [`Writing`]: removed when dropped (the write ended, panicked, or its
/// thread could not start).
#[derive(Debug)]
pub struct WritingEntry {
    writing: std::sync::Arc<Writing>,
    request: EntityId,
}

impl Drop for WritingEntry {
    fn drop(&mut self) {
        self.writing
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tokens
            .remove(&self.request);
    }
}

impl Writing {
    /// A new registry, shared by its host and every write it starts.
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Registers a write of `request`: its token and its entry, which the write's thread holds
    /// to its end. `None` when `request` is already being written or the host is stopping.
    #[must_use]
    pub fn begin(
        self: &std::sync::Arc<Self>,
        request: EntityId,
    ) -> Option<(CancelToken, WritingEntry)> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopping || state.tokens.contains_key(&request) {
            return None;
        }
        let token = CancelToken::new();
        state.tokens.insert(request, token.clone());
        Some((
            token,
            WritingEntry {
                writing: std::sync::Arc::clone(self),
                request,
            },
        ))
    }

    /// The requests being written now.
    #[must_use]
    pub fn ids(&self) -> Vec<EntityId> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tokens
            .keys()
            .copied()
            .collect()
    }

    /// Cancels the write of `request`, if one runs.
    pub fn cancel(&self, request: EntityId) {
        if let Some(token) = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tokens
            .get(&request)
        {
            token.cancel();
        }
    }

    /// Cancels every write and starts no new one (the host is stopping).
    pub fn cancel_all(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stopping = true;
        for token in state.tokens.values() {
            token.cancel();
        }
    }
}

/// What one [`Offload::step`] did: counts, and the meetings whose requests or run records this
/// device wrote as their asker (ids only, T12.81).
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
    /// Meetings the host refused to start writing here (a job of the meeting runs, it is in
    /// use): the next step hands them again.
    pub refused_here: usize,
    /// The meetings whose waiting changed here as their asker: a request withdrawn, a run handed
    /// back or ended after the computer's summary (T12.81), so the window reads them again.
    pub meetings: BTreeSet<EntityId>,
}

/// The offload state of one running process: which requests were already handed to the host,
/// so each is handed once per process (a restart hands an unfinished one again).
#[derive(Debug, Default)]
pub struct Offload {
    handed: Mutex<BTreeSet<EntityId>>,
    /// Requests that need nothing more of this device (done, or declined or withdrawn with a
    /// summary since), so a step does not read their meeting again.
    settled: Mutex<BTreeSet<EntityId>>,
    /// Set by a scan that found nothing to do or to wait for ([`Idle`]); `None` after any other
    /// scan and before the first.
    idle: Mutex<Option<Idle>>,
    /// Scans made (tests: a skipped step makes none).
    #[cfg(test)]
    scans: std::sync::atomic::AtomicUsize,
}

/// What a scan that found nothing to do or to wait for leaves behind (T14.50), so the sync
/// thread's next steps can tell without reading every request that nothing has changed
/// ([`Offload::step_after_tick_into`]).
///
/// Such a scan looked at no request (none is open for this device as asker or helper, none
/// closed is still to be handed back or settled) and wrote nothing. A later scan looks at one
/// only if a request row changed, which is a row this device wrote (the `mark` moves: a new
/// request is a new row, and any write here is stamped after every stamp in the table) or one
/// a sync tick applied (the thread says so), or if this device's clock reads earlier than it did
/// (`at_ms`: a closed request left alone for its age could count as recent again). Nothing else
/// a scan reads can make it look at a request it passed over: a done request no run record
/// waits on is never named by a later record (a record names a request only while it is open),
/// and the requests settled in this process only grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Idle {
    /// The request table's mark when the scan read it (under the same store lock).
    mark: earshot_store::RequestsMark,
    /// This device's clock at the scan.
    at_ms: u64,
}

/// A declined or withdrawn request whose write here started, or that a summary settled, more than
/// this long ago by this device's clock ([`OFFLOAD_TIMES_KEY`]) is not looked at again (7 days, a
/// judgement bounding each step's reads). One whose write here never started is always looked
/// at, however old, and so are open requests: their own rules end them within minutes.
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

    fn handed(&self, request: EntityId) -> bool {
        self.handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&request)
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
        let mut report = StepReport::default();
        self.step_into(services, peers, host, &mut report)
            .map(|()| report)
    }

    /// [`Self::step`] into `report`, which keeps what the step did even when it ends on an error
    /// or a panic after writing (as [`crate::sync::SyncClient::tick_reporting`] does for the
    /// tick), so the window still hears of the meetings it already changed (T12.81).
    ///
    /// # Errors
    /// Store errors; what was done before stays done and stays in `report`.
    pub fn step_into(
        &self,
        services: &Services,
        peers: &mut dyn FnMut() -> Option<Vec<Peer>>,
        host: &dyn OffloadHost,
        report: &mut StepReport,
    ) -> Result<(), RuntimeError> {
        self.step_with(services, peers, host, false, report)
    }

    /// [`Self::step_into`] as the sync thread runs it after each tick (T14.50): the scan of every
    /// request is skipped when the last scan found nothing to do or to wait for ([`Idle`]), no
    /// request row has changed since (`rows_changed`: whether the tick wrote, deleted or
    /// repaired anything here, or cannot say; and the request table's mark, two aggregate reads)
    /// and this device's clock has not gone back. A skipped step does nothing, as the scan would
    /// have, and `report` stays as it was.
    ///
    /// # Errors
    /// As [`Self::step_into`].
    pub fn step_after_tick_into(
        &self,
        services: &Services,
        peers: &mut dyn FnMut() -> Option<Vec<Peer>>,
        host: &dyn OffloadHost,
        rows_changed: bool,
        report: &mut StepReport,
    ) -> Result<(), RuntimeError> {
        if !rows_changed && self.nothing_to_do(services) {
            return Ok(());
        }
        self.step_with(services, peers, host, false, report)
    }

    /// Whether a scan now would find what the last one found: nothing ([`Idle`]). A mark that
    /// cannot be read says nothing, so the scan runs.
    fn nothing_to_do(&self, services: &Services) -> bool {
        let Some(idle) = *self.idle.lock().unwrap_or_else(PoisonError::into_inner) else {
            return false;
        };
        services.clock.now_unix_ms() >= idle.at_ms
            && services
                .store()
                .summary_requests_mark()
                .is_ok_and(|mark| mark == idle.mark)
    }

    fn set_idle(&self, idle: Option<Idle>) {
        *self.idle.lock().unwrap_or_else(PoisonError::into_inner) = idle;
    }

    /// The last step of a sync thread that stops on a problem the user must act on (this device
    /// was removed, another vault, an update needed, no usable address or key) or because sync
    /// was turned off (T11.6c). Nothing
    /// this device writes reaches the helper until sync works again, so every open request it
    /// asked is withdrawn here at once (no claim window, no presence) and its meeting handed to
    /// `host` to write here ([`HereReason::Withdrawn`]); requests asked of this device are left
    /// alone (a claim could not reach the asker, which withdraws on its own rules). A withdrawal
    /// is final, so when sync works again it wins over the request on every device.
    ///
    /// # Errors
    /// Store errors; what was done before stays done.
    pub fn step_sync_stopped(
        &self,
        services: &Services,
        host: &dyn OffloadHost,
    ) -> Result<StepReport, RuntimeError> {
        let mut report = StepReport::default();
        self.step_sync_stopped_into(services, host, &mut report)
            .map(|()| report)
    }

    /// [`Self::step_sync_stopped`] into `report` ([`Self::step_into`]).
    ///
    /// # Errors
    /// Store errors; what was done before stays done and stays in `report`.
    pub fn step_sync_stopped_into(
        &self,
        services: &Services,
        host: &dyn OffloadHost,
        report: &mut StepReport,
    ) -> Result<(), RuntimeError> {
        self.step_with(services, &mut || None, host, true, report)
    }

    fn step_with(
        &self,
        services: &Services,
        peers: &mut dyn FnMut() -> Option<Vec<Peer>>,
        host: &dyn OffloadHost,
        sync_stopped: bool,
        report: &mut StepReport,
    ) -> Result<(), RuntimeError> {
        let (me, now) = (services.store().device(), services.clock.now_unix_ms());
        // Whatever this scan ends with, the last one's finding no longer stands.
        self.set_idle(None);
        #[cfg(test)]
        self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The writes this host runs for other devices, read before the store lock (T14.66): one
        // whose request its asker withdrew is cancelled after the lock is released.
        let writing: BTreeSet<EntityId> = host.writing().into_iter().collect();
        let mut cancel: Vec<EntityId> = Vec::new();
        let (seen, offers, mut times, held, mark) = {
            let db = services.store();
            // Read with the requests, under one lock: a request written after this is seen as a
            // changed mark by the next step.
            let mark = db.summary_requests_mark()?;
            let offers = crate::sync::sync_settings(&db)
                .map(|s| s.offers_summaries)
                .unwrap_or(false);
            let times = read_times(db.local_value(OFFLOAD_TIMES_KEY)?.as_deref());
            let mut seen = Vec::new();
            let mut held: BTreeSet<String> = BTreeSet::new();
            for r in db.summary_requests()? {
                if !r.meta.deleted {
                    held.insert(r.id.to_string());
                }
                // Withdrawn by its asker (authored: a withdrawal any other device wrote is
                // ignored) while this host writes it.
                if r.target_device == me
                    && r.state == RequestState::Withdrawn
                    && !r.meta.deleted
                    && authored(&r)
                    && writing.contains(&r.id)
                {
                    cancel.push(r.id);
                    continue;
                }
                let mine = r.asking_device == me;
                let wanted = if mine {
                    // A done request is read only while a run record waits on it, so the run is
                    // ended with that computer as its writer (T12.63a).
                    r.state != RequestState::Done
                        || (!r.meta.deleted
                            && !self.is_settled(r.id)
                            && crate::summary_order::run::waits_on(&db, r.meeting_id, r.id)?)
                } else {
                    !sync_stopped && r.target_device == me && r.state.is_open()
                };
                if r.meta.deleted
                    || !wanted
                    || !authored(&r)
                    || self.is_settled(r.id)
                    || (!r.state.is_open()
                        && times
                            .closed
                            .get(&r.id.to_string())
                            .is_some_and(|c| c.aged(now)))
                {
                    continue;
                }
                let meeting = db.meeting(r.meeting_id)?;
                let meeting_live = meeting.is_some_and(|m| !m.meta.deleted);
                let summarised_since = meeting_live && summarised_since(&**db, &r)?;
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
            (seen, offers, times, held, mark)
        };
        // With the store lock released.
        for request in cancel {
            host.cancel_writing(request);
        }
        let nothing_seen = seen.is_empty();
        let before = times.clone();
        // Open readings are kept for open requests only, closed ones while the store holds them.
        times.requests.retain(|id, _| {
            seen.iter()
                .any(|s| s.request.state.is_open() && s.request.id.to_string() == *id)
        });
        times.closed.retain(|id, _| held.contains(id));
        let waits_on_claim = !sync_stopped
            && seen.iter().any(|s| {
                s.meeting_live
                    && s.request.asking_device == me
                    && s.request.state == RequestState::Claimed
            });
        let peers = if waits_on_claim { peers() } else { None };
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
                    report,
                    sync_stopped,
                )
            } else {
                self.as_helper(services, &s, now, offers, &mut times, host, report)
            };
            if let Err(e) = done {
                outcome = Err(e);
                break;
            }
        }
        if times != before {
            // Re-read and merged in one transaction: an `ask` since this step read is kept.
            services
                .store()
                .transaction(|tx| store_step_times(tx, &before, &times))?;
        } else if nothing_seen && !sync_stopped && outcome.is_ok() {
            // Nothing looked at and nothing written: the next steps after a tick skip the scan
            // until a request row changes ([`Idle`]).
            self.set_idle(Some(Idle { mark, at_ms: now }));
        }
        outcome
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

    /// This device's closed reading for `r`, first seen now when there is none.
    fn closed<'t>(times: &'t mut Times, r: &SummaryRequest, now: u64) -> &'t mut Closed {
        times
            .closed
            .entry(r.id.to_string())
            .or_insert_with(|| Closed {
                seen_ms: now,
                started_ms: None,
                settled_ms: None,
            })
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
        sync_stopped: bool,
    ) -> Result<(), RuntimeError> {
        let r = &s.request;
        let why = match r.state {
            RequestState::Requested | RequestState::Claimed if sync_stopped => {
                if !move_request(services, r.id, r.state, RequestState::Withdrawn, "")? {
                    return Ok(());
                }
                report.withdrawn += 1;
                report.meetings.insert(r.meeting_id);
                HereReason::Withdrawn
            }
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
                report.meetings.insert(r.meeting_id);
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
                report.meetings.insert(r.meeting_id);
                HereReason::HelperStale
            }
            RequestState::Declined | RequestState::Withdrawn if s.summarised_since => {
                Self::closed(times, r, now).settled_ms.get_or_insert(now);
                self.settle(r.id);
                return Ok(());
            }
            RequestState::Declined => {
                Self::closed(times, r, now);
                HereReason::Declined(r.reason.clone())
            }
            RequestState::Withdrawn => {
                Self::closed(times, r, now);
                HereReason::Withdrawn
            }
            RequestState::Done => {
                // The computer stored the summary: a run waiting on it ends with that computer
                // as its writer.
                if crate::summary_order::run::end_after_done(services, r.meeting_id, r.id)? {
                    report.meetings.insert(r.meeting_id);
                }
                self.settle(r.id);
                return Ok(());
            }
        };
        if self.handed(r.id) {
            return Ok(());
        }
        // The run record decides (docs/summary-order.md 4.3).
        use crate::summary_order::run::{HandBack, OutcomeWord, hand_back};
        let word = match &why {
            HereReason::Declined(_) => OutcomeWord::Declined,
            HereReason::NotClaimed | HereReason::HelperStale | HereReason::Withdrawn => {
                OutcomeWord::NoReply
            }
        };
        let continuation = match hand_back(services, r.meeting_id, r.id, word)? {
            HandBack::Settle => {
                self.settle(r.id);
                return Ok(());
            }
            // A named run's record was ended here: the page shows its account.
            HandBack::Ended => {
                report.meetings.insert(r.meeting_id);
                self.settle(r.id);
                return Ok(());
            }
            HandBack::Legacy => Continuation::Legacy,
            HandBack::ContinueOrder => Continuation::Order { word },
        };
        if self.hand_once(r.id) {
            let answer = host.continue_run(WriteHere {
                meeting: r.meeting_id,
                request: r.id,
                helper: r.target_device,
                why,
                continuation,
            });
            match answer {
                ContinueAnswer::Started => {
                    report.written_here += 1;
                    report.meetings.insert(r.meeting_id);
                    Self::closed(times, r, now).started_ms.get_or_insert(now);
                }
                // Refused (a job of the meeting runs, the meeting is in use): the next step tries
                // again.
                ContinueAnswer::Refused => {
                    self.unhand(r.id);
                    report.refused_here += 1;
                }
                ContinueAnswer::Settled => self.settle(r.id),
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
    /// Cancelled, nothing stored and nothing written to the request: either the host is stopping
    /// (the request stays claimed and is handed again at the next start) or its asker withdrew it
    /// ([`Writing::cancel`], T14.66: nothing more to do).
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
