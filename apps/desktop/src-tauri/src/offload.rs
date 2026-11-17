//! Summary offload in the app (T11.3db; docs/sync.md section 10, `earshot_runtime::offload`).
//!
//! - The Summaries page lists each paired computer that writes summaries for the others as
//!   "On <device> (<writer kind>)" beside this computer's writer ([`summary_helpers`],
//!   [`set_summary_helper`]); the choice is device-local.
//! - A meeting's summary step decides once, when the summary is due
//!   ([`MeetingDriver::set_offload_choice`](crate::meeting::MeetingDriver::set_offload_choice)):
//!   the meeting shows "Waiting for <device>" ([`waiting_copy`]) or, when this computer's writer
//!   ran because the chosen one was not available, banner [`OFFLOAD_BANNER_KEY`]
//!   ([`unavailable_copy`]).
//! - Re-summarise by hand offers "Here" (the ordinary job) and "On <device>"
//!   ([`ask_summary_helper`]).
//! - [`HostOffload`] does the sync thread's offload work: a meeting handed back is re-summarised
//!   here with a note ([`written_here_copy`]); a request from another computer is written through
//!   this computer's ordinary summary setup in the meeting driver's queue.
//!
//! All copy is fixed, from `fixtures/ui/offload_copy_cases.json` (read by the UI mock too). A
//! device name is the user's own, from the device's presence; no meeting content appears here.

use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_core::identity::DeviceId;
use earshot_core::model::SummaryRequest;
use earshot_runtime::Services;
use earshot_runtime::keep_local::SummaryPrivacy;
use earshot_runtime::meeting_queue::MeetingQueue;
use earshot_runtime::offload::{
    Continuation, ContinueAnswer, HelperWriter, HereReason, OffloadChoice, OffloadHost,
    Unavailable, WriteHere, WriterKind, reason,
};
use earshot_runtime::sync::ModelPlace;
use serde::Serialize;
use specta::Type;

use crate::ipc::{AppState, IpcError, blocking};
use crate::meeting_events::{MeetingEvent, WarningKind};
use crate::sync_settings::SyncHost;

/// The banner key of a summary this computer wrote because the chosen one was not available.
pub const OFFLOAD_BANNER_KEY: &str = "summary-offload";

/// What the page calls each writer kind (the table's `writers`).
#[must_use]
pub const fn writer_label(kind: WriterKind) -> &'static str {
    match kind {
        WriterKind::Local => "local model",
        WriterKind::Anthropic => "Claude with an API key",
        WriterKind::OpenaiCompatible => "own server",
        WriterKind::ClaudeCode => "Claude Code",
        WriterKind::Codex => "Codex",
    }
}

/// What the page calls a summary worker's own local model by where it runs (the table's
/// `local_model_places`, T11.6a).
#[must_use]
pub const fn local_model_place_label(place: ModelPlace) -> &'static str {
    match place {
        ModelPlace::Processor => "local model, processor",
        ModelPlace::GraphicsCard => "local model, graphics card",
    }
}

/// The label of another computer's writer: its writer kind ([`writer_label`]), or for a summary
/// worker's own local model with a known place, that place ([`local_model_place_label`]).
/// The place is used only with [`WriterKind::Local`].
#[must_use]
pub const fn computer_writer_label(kind: WriterKind, place: Option<ModelPlace>) -> &'static str {
    match (kind, place) {
        (WriterKind::Local, Some(p)) => local_model_place_label(p),
        _ => writer_label(kind),
    }
}

/// `{device}` at the start of a sentence when the name is not known (the table's `unnamed`).
pub const UNNAMED: &str = "Your other computer";
/// `{device}` inside a sentence when the name is not known (the table's `unnamed_inline`).
pub const UNNAMED_INLINE: &str = "your other computer";

fn named(name: Option<&str>, unnamed: &str) -> String {
    name.filter(|n| !n.trim().is_empty())
        .unwrap_or(unnamed)
        .to_owned()
}

/// The template of each reason the chosen computer was not asked, for a summary this computer
/// stored instead (the table's `unavailable`). Shown only when a summary was stored here.
#[must_use]
pub const fn unavailable_template(why: Unavailable) -> &'static str {
    match why {
        Unavailable::SyncOff => "Sync is off, so this computer wrote the summary.",
        Unavailable::Unreachable => {
            "The sync server could not be reached, so this computer wrote the summary."
        }
        Unavailable::Offline => "{device} is not online, so this computer wrote the summary.",
        Unavailable::NotOffering => {
            "{device} no longer writes summaries for your other computers, so this computer wrote the summary."
        }
        Unavailable::NoWriter => {
            "{device} has no summary writer set up, so this computer wrote the summary."
        }
        Unavailable::Recording => {
            "{device} is recording a meeting, so this computer wrote the summary."
        }
        Unavailable::NoFinalTranscript => {
            "This meeting has no final transcript to send, so this computer wrote the summary."
        }
        Unavailable::NotAsked => "{device} could not be asked, so this computer wrote the summary.",
    }
}

/// The template of each reason the chosen computer was not asked, claiming nothing about a
/// summary (the table's `not_asked`): for a summary step that stored none here, and a refused
/// re-summarise "On <device>".
#[must_use]
pub const fn not_asked_template(why: Unavailable) -> &'static str {
    match why {
        Unavailable::SyncOff => "Sync is off, so no other computer was asked.",
        Unavailable::Unreachable => {
            "The sync server could not be reached, so no other computer was asked."
        }
        Unavailable::Offline => "{device} is not online, so it was not asked.",
        Unavailable::NotOffering => {
            "{device} no longer writes summaries for your other computers, so it was not asked."
        }
        Unavailable::NoWriter => "{device} has no summary writer set up, so it was not asked.",
        Unavailable::Recording => "{device} is recording a meeting, so it was not asked.",
        Unavailable::NoFinalTranscript => {
            "This meeting has no final transcript to send, so no other computer was asked."
        }
        Unavailable::NotAsked => "{device} could not be asked.",
    }
}

/// The table word of `why` (the copy table's key, read by the tests).
#[cfg(test)]
#[must_use]
pub const fn unavailable_word(why: Unavailable) -> &'static str {
    match why {
        Unavailable::SyncOff => "sync_off",
        Unavailable::Unreachable => "unreachable",
        Unavailable::Offline => "offline",
        Unavailable::NotOffering => "not_offering",
        Unavailable::NoWriter => "no_writer",
        Unavailable::Recording => "recording",
        Unavailable::NoFinalTranscript => "no_final_transcript",
        Unavailable::NotAsked => "not_asked",
    }
}

/// Why this computer wrote the summary although a computer was chosen.
#[must_use]
pub fn unavailable_copy(why: Unavailable, name: Option<&str>) -> String {
    unavailable_template(why).replace("{device}", &named(name, UNNAMED))
}

/// The template of each reason this computer writes a summary it had asked another computer for
/// (the table's `written_here`, by [`here_word`]): the re-summary job's first warning, sent before
/// the job runs, so it says what is being done, not that it was done.
#[must_use]
pub const fn written_here_template(why: &HereReason) -> &'static str {
    match why {
        HereReason::NotClaimed => {
            "{device} did not answer, so this computer writes the summary instead."
        }
        HereReason::HelperStale => {
            "{device} went offline before it finished, so this computer writes the summary instead."
        }
        HereReason::Declined(_) => {
            "{device} could not write the summary, so this computer writes it instead."
        }
        HereReason::Withdrawn => {
            "{device} did not write the summary, so this computer writes it instead."
        }
    }
}

/// The table word of `why` (the copy table's key, read by the tests).
#[cfg(test)]
#[must_use]
pub const fn here_word(why: &HereReason) -> &'static str {
    match why {
        HereReason::NotClaimed => "not_claimed",
        HereReason::HelperStale => "helper_stale",
        HereReason::Declined(_) => "declined",
        HereReason::Withdrawn => "withdrawn",
    }
}

/// Why this computer writes a summary it had asked another computer for.
#[must_use]
pub fn written_here_copy(why: &HereReason, name: Option<&str>) -> String {
    written_here_template(why).replace("{device}", &named(name, UNNAMED))
}

/// The template of the meeting's line while a request is open (the table's `waiting`).
pub const WAITING_TEMPLATE: &str = "Waiting for {device} to write the summary.";

/// The meeting's line while it waits for `name`'s summary.
#[must_use]
pub fn waiting_copy(name: Option<&str>) -> String {
    WAITING_TEMPLATE.replace("{device}", &named(name, UNNAMED_INLINE))
}

/// The banner of a summary step whose chosen computer was not asked; `None` for any other
/// choice. It says this computer wrote the summary only when `summary` is a stored one; for a
/// summary skipped, failed or cancelled it says only that the computer was not asked.
#[must_use]
pub fn offload_banner(
    choice: &OffloadChoice,
    summary: &earshot_runtime::host::SummaryResult,
) -> Option<MeetingEvent> {
    match choice {
        OffloadChoice::HereInstead { name, why, .. } => Some(MeetingEvent::Warning {
            key: OFFLOAD_BANNER_KEY.into(),
            kind: WarningKind::Backend,
            message: if matches!(summary, earshot_runtime::host::SummaryResult::Stored { .. }) {
                unavailable_copy(*why, name.as_deref())
            } else {
                not_asked_copy(*why, name.as_deref())
            },
        }),
        OffloadChoice::Here | OffloadChoice::Ask { .. } => None,
    }
}

/// One paired computer the Summaries page can ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryHelperView {
    /// Its id.
    pub id: String,
    /// Its name, from its presence; `None` until it sends one (the page shows the short id).
    pub name: Option<String>,
    /// The first eight hex digits of its id.
    pub short_id: String,
    /// The kind of writer it uses (fixed copy, [`computer_writer_label`]: a summary worker's
    /// own local model names where it runs); `None` when it has none.
    pub writer: Option<String>,
    /// Whether it writes summaries for the others now.
    pub offers: bool,
    /// Whether it is online now.
    pub online: bool,
    /// Whether it is recording now.
    pub recording: bool,
    /// Whether it would be asked now: online, offering, with a writer, not recording.
    pub available: bool,
    /// Whether it is the chosen one.
    pub chosen: bool,
}

/// The Summaries page's offload part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryHelpersView {
    /// Sync is set up and on, so another computer can be asked.
    pub sync_ready: bool,
    /// The chosen computer's id, if any.
    pub chosen: Option<String>,
    /// The computers that offer summaries, and the chosen one whatever it offers now.
    pub helpers: Vec<SummaryHelperView>,
    /// Why the computers could not be read now (fixed copy); the chosen one is still listed.
    pub problem: Option<String>,
}

/// The device id in `text`.
///
/// # Errors
/// [`IpcError::invalid`] for text that is not one.
pub(crate) fn device_id(text: &str) -> Result<DeviceId, IpcError> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .map_err(|_| IpcError::invalid("That computer is not one Earshot knows."))
}

/// The name and writer kind each of the user's other computers last said in its presence, as
/// this process heard it (memory only; for the copy of the order of summary writers, T12.63b).
static COMPUTERS: std::sync::Mutex<
    std::collections::BTreeMap<DeviceId, crate::summary_order::Named>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Remembers what `device`'s presence said (a known name or kind is kept when `None`).
pub(crate) fn remember_computer(
    device: DeviceId,
    name: Option<String>,
    writer: Option<WriterKind>,
) {
    let mut all = COMPUTERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remember_in(all.entry(device).or_default(), name, writer);
}

fn remember_in(
    known: &mut crate::summary_order::Named,
    name: Option<String>,
    writer: Option<WriterKind>,
) {
    if let Some(n) = name.filter(|n| !n.trim().is_empty()) {
        known.name = Some(n);
    }
    if let Some(w) = writer {
        // A place is known only with the writer it came with (T11.6a).
        if known.writer != Some(w) {
            known.place = None;
        }
        known.writer = Some(w);
    }
}

/// Remembers what `device`'s presence said, read from the vault's device list: its name, writer
/// kind and where its own local model runs, which replaces any place heard before (T11.6a).
pub(crate) fn remember_presence(
    device: DeviceId,
    name: Option<String>,
    writer: Option<WriterKind>,
    place: Option<ModelPlace>,
) {
    let mut all = COMPUTERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let known = all.entry(device).or_default();
    remember_in(known, name, writer);
    known.place = place.filter(|_| known.writer == Some(WriterKind::Local));
}

/// What this process has heard of the user's other computers.
#[must_use]
pub(crate) fn known_computers() -> std::collections::BTreeMap<DeviceId, crate::summary_order::Named>
{
    COMPUTERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn short(device: DeviceId) -> String {
    device
        .as_uuid()
        .as_bytes()
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The Summaries page's offload part now (asks the server for the computers).
///
/// # Errors
/// Fixed copy of a store failure.
pub fn helpers_view(services: &Services, host: &SyncHost) -> Result<SummaryHelpersView, IpcError> {
    let (chosen, on) = {
        let db = services.store();
        let chosen = earshot_runtime::offload::chosen_helper(&db)?;
        let on = earshot_runtime::sync::sync_settings(&db).is_ok_and(|s| s.on);
        (chosen, on)
    };
    let ready = on && host.vault().has().unwrap_or(false);
    let only_chosen = |problem: Option<String>| SummaryHelpersView {
        sync_ready: ready,
        chosen: chosen.map(|d| d.to_string()),
        helpers: chosen
            .map(|d| SummaryHelperView {
                id: d.to_string(),
                name: None,
                short_id: short(d),
                writer: None,
                offers: false,
                online: false,
                recording: false,
                available: false,
                chosen: true,
            })
            .into_iter()
            .collect(),
        problem,
    };
    if !ready {
        return Ok(only_chosen(None));
    }
    let devices = match earshot_runtime::sync::vault_devices(services, host.vault()) {
        Ok(d) => d,
        Err(p) => {
            return Ok(only_chosen(Some(
                crate::sync_settings::problem_copy(p).to_owned(),
            )));
        }
    };
    for d in devices.iter().filter(|d| !d.this_device) {
        remember_presence(d.device, d.name.clone(), d.writer_kind, d.model_place);
    }
    let helpers = devices
        .into_iter()
        .filter(|d| !d.this_device && (d.offers_summaries || chosen == Some(d.device)))
        .map(|d| SummaryHelperView {
            id: d.device.to_string(),
            available: d.online && d.offers_summaries && !d.recording && d.writer_kind.is_some(),
            name: d.name,
            short_id: d.short_id,
            writer: d
                .writer_kind
                .map(|k| computer_writer_label(k, d.model_place).to_owned()),
            offers: d.offers_summaries,
            online: d.online,
            recording: d.recording,
            chosen: chosen == Some(d.device),
        })
        .collect::<Vec<_>>();
    let mut view = SummaryHelpersView {
        sync_ready: true,
        chosen: chosen.map(|d| d.to_string()),
        helpers,
        problem: None,
    };
    // A chosen computer the server no longer lists (removed) still shows, so it can be unchosen.
    if chosen.is_some() && !view.helpers.iter().any(|h| h.chosen) {
        view.helpers.extend(only_chosen(None).helpers);
    }
    Ok(view)
}

/// Chooses the computer this one asks (`None`: none, this computer's writer as before).
///
/// # Errors
/// [`IpcError::invalid`] for an id that is not a device's, or this computer's own; store errors.
pub fn set_helper(
    services: &Services,
    host: &SyncHost,
    device: Option<&str>,
) -> Result<SummaryHelpersView, IpcError> {
    let device = device.map(device_id).transpose()?;
    earshot_runtime::offload::set_chosen_helper(services, device)
        .map_err(|_| IpcError::invalid("This computer cannot write summaries for itself."))?;
    helpers_view(services, host)
}

/// Asks `device` to write `meeting`'s summary now (re-summarise "On <device>"): presence is read
/// and the request written only when the computer would be asked; returns the meeting's waiting
/// line.
///
/// # Errors
/// [`IpcError::invalid`] for an unknown id; the reason it would not be asked (fixed copy,
/// [`unavailable_copy`]); `busy` while the meeting records, finishes or runs a job; store errors.
pub fn ask_helper(
    services: &Services,
    host: &SyncHost,
    meeting: &str,
    device: &str,
) -> Result<String, IpcError> {
    let (meeting, device) = (crate::detail::parse_id(meeting)?, device_id(device)?);
    // The work after a meeting or a job holds it; asking now would race that summary.
    let _lease = services
        .lease_meeting_alone(meeting, earshot_runtime::MeetingJob::Processing)
        .map_err(IpcError::from)?;
    let ready = {
        let db = services.store();
        earshot_runtime::sync::sync_settings(&db).is_ok_and(|s| s.on)
    } && host.vault().has().unwrap_or(false);
    let choice = if ready {
        match earshot_runtime::sync::SyncClient::new(services.clone(), host.vault())
            .and_then(|c| c.peers())
        {
            Ok(peers) => earshot_runtime::offload::decide(Some(device), Some(&peers), false),
            Err(_) => earshot_runtime::offload::decide(Some(device), None, true),
        }
    } else {
        earshot_runtime::offload::decide(Some(device), None, false)
    };
    // One open request per meeting: the same computer again answers with it, another is refused.
    let waiting = {
        let db = services.store();
        earshot_runtime::offload::waiting_for(&db, db.device(), meeting)?
    };
    match waiting {
        Some(t) if t == device => {
            let name = match &choice {
                OffloadChoice::Ask { name, .. } => Some(name.as_str()),
                OffloadChoice::HereInstead { name, .. } => name.as_deref(),
                OffloadChoice::Here => None,
            };
            return Ok(waiting_copy(name));
        }
        Some(_) => return Err(IpcError::new(OFFLOAD_ERROR_KIND, ALREADY_WAITING)),
        None => {}
    }
    let name = match &choice {
        OffloadChoice::Ask { name, writer, .. } => {
            remember_computer(device, Some(name.clone()), Some(*writer));
            name.clone()
        }
        OffloadChoice::HereInstead { why, name, .. } => {
            return Err(IpcError::new(
                OFFLOAD_ERROR_KIND,
                not_asked_copy(*why, name.as_deref()),
            ));
        }
        OffloadChoice::Here => return Err(IpcError::internal()),
    };
    // A named run (T12.63b, docs/summary-order.md 5): its record names the request, so a
    // hand-back ends the run and nothing is written on this computer instead.
    let writers = AskOnly { device, choice };
    let out = earshot_runtime::summary_order::run::run_summary_order(
        services,
        meeting,
        earshot_runtime::summary_order::run::RunStart::Named(
            earshot_runtime::summary_order::OrderStep::Computer(device),
        ),
        &earshot_runtime::summary_order::run::RunPlan {
            writers: &writers,
            local_not_now: &|| false,
            order: None,
        },
        &CancelToken::new(),
    );
    match out.summary {
        earshot_runtime::host::SummaryResult::Offloaded { .. } => {
            host.nudge();
            Ok(waiting_copy(Some(&name)))
        }
        earshot_runtime::host::SummaryResult::Failed { error, .. } => Err(IpcError::from(error)),
        // Not asked after all: why, from what the run recorded for its one step.
        _ => {
            let word = out.steps.first().and_then(|v| match v.result {
                earshot_runtime::summary_order::run::StepResult::Moved(w) => Some(w),
                _ => None,
            });
            let has_final = {
                use earshot_core::model::MeetingRepo;
                services
                    .store()
                    .has_live_segment(meeting, earshot_core::model::TranscriptPass::Final)
                    .unwrap_or(false)
            };
            Err(IpcError::new(
                OFFLOAD_ERROR_KIND,
                not_asked_copy(not_asked_reason(word, has_final), Some(&name)),
            ))
        }
    }
}

/// Why a named computer run did not ask its computer, from the outcome word its step recorded:
/// the presence words name themselves; a step that failed to ask (`failed`, which the run records
/// both for a meeting with no final transcript and for a request it could not write) is the
/// former only when the meeting has no final transcript.
#[must_use]
pub(crate) fn not_asked_reason(
    word: Option<earshot_runtime::summary_order::run::OutcomeWord>,
    has_final: bool,
) -> Unavailable {
    use earshot_runtime::summary_order::run::OutcomeWord;
    match word {
        Some(OutcomeWord::Offline | OutcomeWord::NoReply) => Unavailable::Offline,
        Some(OutcomeWord::Recording) => Unavailable::Recording,
        Some(OutcomeWord::NotOffering) => Unavailable::NotOffering,
        Some(OutcomeWord::NoWriter) => Unavailable::NoWriter,
        _ if !has_final => Unavailable::NoFinalTranscript,
        _ => Unavailable::NotAsked,
    }
}

/// The writers of a named computer run: its own computer answers as already decided, any other
/// device reads as not asked (the run's trust boundary holds even here), and no writer step
/// exists.
pub(crate) struct AskOnly {
    /// The computer the user named.
    pub(crate) device: DeviceId,
    /// What was decided about it.
    pub(crate) choice: OffloadChoice,
}

impl earshot_runtime::summary_order::run::StepWriters for AskOnly {
    fn writer(
        &self,
        _: &Services,
        _: earshot_runtime::summary_order::WriterStep,
    ) -> earshot_runtime::summary_order::run::Built {
        earshot_runtime::summary_order::run::Built::Stop(earshot_runtime::RuntimeError::Internal(
            "a named computer run has no writer step",
        ))
    }

    fn computer(&self, _: &Services, device: DeviceId) -> OffloadChoice {
        if device == self.device {
            self.choice.clone()
        } else {
            earshot_runtime::offload::OffloadChoice::HereInstead {
                device,
                name: None,
                why: Unavailable::NotAsked,
            }
        }
    }
}

/// The IPC error kind of a computer that would not be asked.
pub const OFFLOAD_ERROR_KIND: &str = "offload";

/// Why the chosen computer was not asked, claiming nothing about a summary
/// ([`not_asked_template`]).
#[must_use]
pub fn not_asked_copy(why: Unavailable, name: Option<&str>) -> String {
    not_asked_template(why).replace("{device}", &named(name, UNNAMED))
}

/// The refusal of re-summarise "On <device>" while the meeting already waits for another
/// computer (the table's `already_waiting`).
pub const ALREADY_WAITING: &str =
    "This meeting already waits for another computer to write its summary.";

/// The Summaries page's offload part (asks the server).
#[tauri::command]
#[specta::specta]
pub async fn summary_helpers(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<SummaryHelpersView, IpcError> {
    let services = state.services()?;
    let host = Arc::clone(&sync);
    blocking(move || helpers_view(&services, &host)).await
}

/// Chooses the computer this one asks to write its summaries (`None`: none).
#[tauri::command]
#[specta::specta]
pub async fn set_summary_helper(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    device: Option<String>,
) -> Result<SummaryHelpersView, IpcError> {
    let services = state.services()?;
    let host = Arc::clone(&sync);
    blocking(move || set_helper(&services, &host, device.as_deref())).await
}

/// Asks another computer to write a meeting's summary now; returns the meeting's waiting line.
#[tauri::command]
#[specta::specta]
pub async fn ask_summary_helper(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    meeting: String,
    device: String,
) -> Result<String, IpcError> {
    let services = state.services()?;
    let host = Arc::clone(&sync);
    blocking(move || ask_helper(&services, &host, &meeting, &device)).await
}

/// How a request from another computer gets its writer (production: [`HostHelperWork`]).
pub trait HelperWork: Send + Sync {
    /// The writer for a meeting of `privacy`, through this computer's ordinary summary setup.
    fn writer(&self, services: &Services, privacy: SummaryPrivacy) -> HelperWriter;
}

/// The production [`HelperWork`] (docs/summary-order.md 4.6): exactly the writer kind this
/// computer's presence advertised, the first writer step of its order
/// (`summary_order::run::helper_writer` over the production writers with the app's models, key
/// store and agent CLIs); never a computer step or a next step. A kept-local meeting without a
/// local model declines `kept_local_no_model`; no writer (no summary, a CLI not signed in, a key
/// that cannot be read) declines `no_writer`.
pub struct HostHelperWork {
    /// What a writer step reads when it is built.
    pub probes: crate::summary_order::SummaryProbes,
    /// Where the models are (`None`: the app's own folder).
    pub models: Option<earshot_runtime::backends::ModelDir>,
}

impl HostHelperWork {
    /// The app's: the installed key store and CLIs, the app's models folder.
    #[must_use]
    pub fn installed() -> Self {
        Self {
            probes: crate::summary_order::SummaryProbes::installed(),
            models: None,
        }
    }
}

impl HelperWork for HostHelperWork {
    fn writer(&self, svc: &Services, privacy: SummaryPrivacy) -> HelperWriter {
        let models = self
            .models
            .clone()
            .unwrap_or_else(|| earshot_runtime::host::model_dir(&svc.data));
        crate::summary_order::with_writers(
            &models,
            &*self.probes.keys,
            &*self.probes.claude,
            &*self.probes.codex,
            None,
            |writers| earshot_runtime::summary_order::run::helper_writer(svc, writers, privacy),
        )
    }
}

/// The app's [`OffloadHost`]: a meeting handed back is re-summarised here (a job, with the
/// reason as its first warning); a request from another computer is written on its own thread
/// (`earshot-offload`) in the meeting driver's queue, holding a work pass (with none to be had,
/// it is declined `busy` and the asker writes it).
pub struct HostOffload {
    services: Services,
    jobs: crate::jobs::JobRunner,
    queue: MeetingQueue,
    gate: crate::activity::WorkGate,
    work: Arc<dyn HelperWork>,
    /// How the `earshot-offload` thread starts (a seam: tests make it fail).
    spawn: crate::slot_job::Spawn,
}

impl HostOffload {
    /// The host over the app's services, job runner, meeting queue and work gate.
    #[must_use]
    pub fn new(
        services: Services,
        jobs: crate::jobs::JobRunner,
        queue: MeetingQueue,
        gate: crate::activity::WorkGate,
        work: Arc<dyn HelperWork>,
    ) -> Self {
        Self {
            services,
            jobs,
            queue,
            gate,
            work,
            spawn: crate::slot_job::spawn_named,
        }
    }

    /// The host with another way to start its thread (tests).
    #[cfg(test)]
    #[must_use]
    pub fn with_spawn(mut self, spawn: crate::slot_job::Spawn) -> Self {
        self.spawn = spawn;
        self
    }
}

impl OffloadHost for HostOffload {
    fn continue_run(&self, job: WriteHere) -> ContinueAnswer {
        let (start, note) = match job.continuation {
            // A request asked before the order of summary writers (T12.63a): written here as
            // before, with the order's first writer step under the meeting's privacy.
            Continuation::Legacy => (
                crate::jobs::SummaryStart::Legacy,
                written_here_copy(&job.why, None),
            ),
            // The run waiting on this request goes on from its copied steps (T12.63b), never
            // from the order as it is now.
            Continuation::Order { word } => (
                crate::jobs::SummaryStart::Continue {
                    request: job.request,
                    word,
                },
                crate::summary_order::continue_note(
                    job.helper,
                    &crate::summary_order::StepContext::read(&self.services),
                ),
            ),
        };
        // A job of the meeting that already runs, the model check or a meeting in use refuses:
        // the next step hands it again.
        if self
            .jobs
            .start_summary(&self.services, job.meeting, start, Some(note))
            .is_ok()
        {
            ContinueAnswer::Started
        } else {
            ContinueAnswer::Refused
        }
    }

    fn write_for(&self, request: SummaryRequest) {
        let (svc, queue, gate, work) = (
            self.services.clone(),
            self.queue.clone(),
            self.gate.clone(),
            Arc::clone(&self.work),
        );
        let id = request.id;
        let spawned = (self.spawn)(
            "earshot-offload",
            Box::new(move || {
                // One outcome whatever happens: a panic declines `failed`, so the asker writes it.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let pass = gate.enter_work();
                    let busy = pass.is_err();
                    let _ = earshot_runtime::offload::write_for_request(
                        &svc,
                        &queue,
                        &request,
                        &|privacy| {
                            if busy {
                                HelperWriter::Decline(reason::BUSY)
                            } else {
                                work.writer(&svc, privacy)
                            }
                        },
                        &CancelToken::new(),
                    );
                    drop(pass);
                }));
                if ran.is_err() {
                    let _ = earshot_runtime::offload::decline(&svc, id, reason::FAILED);
                }
            }),
        );
        if spawned.is_err() {
            let _ = earshot_runtime::offload::decline(&self.services, id, reason::FAILED);
        }
    }
}

#[cfg(test)]
#[path = "offload_tests.rs"]
mod tests;
