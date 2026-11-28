//! Summary writers in order (T12.63a, docs/summary-order.md): the one ordered list of who writes
//! a meeting's summary, up to three steps, each a writer on this computer or one of the user's
//! other computers.
//!
//! - **Stored** as device-local `settings/summary-order` ([`SUMMARY_ORDER_KEY`],
//!   `version u32 LE | JSON`): `{"steps": [...]}` or `{"none": true}`; a step is
//!   `{"writer": "local" | "endpoint" | "claude-code" | "codex"}` or `{"computer": "<device>"}`.
//! - **Written** only by [`set_summary_order`], which also writes the mirror an older build reads
//!   ([`SummaryOrder::mirror`]: `settings/summary-writer` the first writer step or `none`,
//!   `settings/summary-offload-device` the computer only when it is step 1) in the same
//!   transaction, so the old keys have exactly one writer.
//!   [`crate::summary_settings::set_summary_settings`] (which stores the order as read),
//!   [`crate::offload::set_chosen_helper`] ([`SummaryOrder::with_helper`]) and
//!   [`set_online_service`] go through it.
//! - **Read** only through [`summary_order`]: with no stored order, or old keys that differ from
//!   the mirror of the stored order (a downgraded build changed them), the old keys win
//!   ([`SummaryOrder::from_old`]); an unreadable or refused stored order reads as the old keys
//!   with a note ([`UNREADABLE_ORDER`]), never the stored text. A stored order holding more than
//!   one online writer (an older build ticked a second) keeps the first, with a note
//!   ([`SECOND_ONLINE_DROPPED`]), before the read checks (T14.72, docs/summary-order.md 12.5).
//! - **The one online service** (T14.72, docs/summary-order.md 12): an order holds at most one
//!   online writer ([`WriterStep::is_online`]), and it is the service picked ([`Service`],
//!   `settings/online-service`, [`ONLINE_SERVICE_KEY`]). The pick is read only through
//!   [`online_service`] and written with the order, its mirror and (for a new endpoint service)
//!   the endpoint's provider in one transaction, the only writer of the key and the provider.
//!
//! How a meeting runs the order (the per-meeting run record, the move-on table, computer steps
//! as suspensions) is [`run`].

use std::fmt;

use earshot_core::identity::DeviceId;
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::keep_local::SummaryPrivacy;
use crate::local::DeviceRecord;
use crate::local::{Setting, read_setting};
use crate::summary_settings::{
    EndpointProvider, SUMMARY_WRITER_KEY, SummarySettings, SummarySettingsError, SummaryWriter,
};

pub mod run;

/// `local_state` key of the order.
pub const SUMMARY_ORDER_KEY: &str = "settings/summary-order";

/// The stored order's format version.
pub const ORDER_VERSION: u32 = 1;

/// The most steps an order holds.
pub const MAX_STEPS: usize = 3;

/// The note of a stored order this version cannot read or refuses (fixed copy: the old keys
/// were read instead).
pub const UNREADABLE_ORDER: &str =
    "the stored order of summary writers cannot be read, so the earlier summary setting was used";

/// `local_state` key of the one online service picked (T14.72): `claude-code`, `codex` or
/// `endpoint` ([`WriterStep::word`]). Which endpoint service it is comes from the endpoint's
/// stored provider, never from this key.
pub const ONLINE_SERVICE_KEY: &str = "settings/online-service";

/// The note of a stored order that held more than one online writer: only the first is used
/// (fixed copy, `page.second_online_dropped` in `fixtures/ui/summary_order_cases.json`).
pub const SECOND_ONLINE_DROPPED: &str =
    "Your order of summary writers held more than one online service, so only the first is used.";

/// The note of a stored online service this version does not know (fixed copy, never the stored
/// text; `page.unknown_service`).
pub const UNKNOWN_SERVICE: &str =
    "The stored online service is not one this version knows, so none is chosen.";

/// Why an online writer that is not the pick was refused (fixed copy, `page.not_the_service`).
pub const NOT_THE_SERVICE: &str = "Only the online service you chose can write summaries.";

/// Why settings from an older read of the page were refused (fixed copy,
/// `page.settings_changed`).
pub const SETTINGS_CHANGED: &str = "The summary settings changed. Check them and try again.";

/// A writer on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriterStep {
    /// The local summary model.
    Local,
    /// The user's one endpoint (OpenAI-compatible or Anthropic).
    Endpoint,
    /// Claude through the user's Claude Code.
    ClaudeCode,
    /// OpenAI through the user's Codex.
    Codex,
}

impl WriterStep {
    /// Every writer.
    pub const ALL: [Self; 4] = [Self::Local, Self::Endpoint, Self::ClaudeCode, Self::Codex];

    /// The stored word (the same as [`SummaryWriter::as_str`]).
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Endpoint => "endpoint",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// The page's writer of this step.
    #[must_use]
    pub const fn writer(self) -> SummaryWriter {
        match self {
            Self::Local => SummaryWriter::OnThisComputer,
            Self::Endpoint => SummaryWriter::OwnEndpoint,
            Self::ClaudeCode => SummaryWriter::ClaudeCode,
            Self::Codex => SummaryWriter::Codex,
        }
    }

    /// The step of `writer` (`None` for no summary).
    #[must_use]
    pub const fn of_writer(writer: SummaryWriter) -> Option<Self> {
        match writer {
            SummaryWriter::OnThisComputer => Some(Self::Local),
            SummaryWriter::OwnEndpoint => Some(Self::Endpoint),
            SummaryWriter::ClaudeCode => Some(Self::ClaudeCode),
            SummaryWriter::Codex => Some(Self::Codex),
            SummaryWriter::NoSummary => None,
        }
    }

    /// Whether this writer sends the transcript off this computer.
    #[must_use]
    pub const fn is_remote(self) -> bool {
        !matches!(self, Self::Local)
    }

    /// Whether this is an online writer (T14.72): the endpoint, Claude Code or Codex. An order
    /// holds at most one, and it is the [`Service`] picked.
    #[must_use]
    pub const fn is_online(self) -> bool {
        matches!(self, Self::Endpoint | Self::ClaudeCode | Self::Codex)
    }

    /// The writer of a stored word ([`WriterStep::word`]).
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.word() == word)
    }
}

/// The one online service the user picked (T14.72, docs/summary-order.md 12.3): what the
/// Summaries page's picker names. Anthropic API and Your own server are the one endpoint
/// configuration, told apart by its provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    /// Claude through the user's Claude Code.
    ClaudeCode,
    /// OpenAI through the user's Codex.
    Codex,
    /// The endpoint on the Anthropic API.
    AnthropicApi,
    /// The endpoint on the user's own (OpenAI-compatible) server.
    OwnServer,
}

impl Service {
    /// Every service, in the picker's order.
    pub const ALL: [Self; 4] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::AnthropicApi,
        Self::OwnServer,
    ];

    /// The writer step of this service.
    #[must_use]
    pub const fn step(self) -> WriterStep {
        match self {
            Self::ClaudeCode => WriterStep::ClaudeCode,
            Self::Codex => WriterStep::Codex,
            Self::AnthropicApi | Self::OwnServer => WriterStep::Endpoint,
        }
    }

    /// The endpoint provider this service stores, `None` for Claude Code and Codex.
    #[must_use]
    pub const fn provider(self) -> Option<EndpointProvider> {
        match self {
            Self::AnthropicApi => Some(EndpointProvider::Anthropic),
            Self::OwnServer => Some(EndpointProvider::OpenAiCompatible),
            Self::ClaudeCode | Self::Codex => None,
        }
    }

    /// The service of online writer `step`, the endpoint's by `provider`; `None` for the local
    /// model.
    #[must_use]
    pub const fn of_step(step: WriterStep, provider: EndpointProvider) -> Option<Self> {
        match (step, provider) {
            (WriterStep::Local, _) => None,
            (WriterStep::ClaudeCode, _) => Some(Self::ClaudeCode),
            (WriterStep::Codex, _) => Some(Self::Codex),
            (WriterStep::Endpoint, EndpointProvider::Anthropic) => Some(Self::AnthropicApi),
            (WriterStep::Endpoint, EndpointProvider::OpenAiCompatible) => Some(Self::OwnServer),
        }
    }
}

/// One step of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrderStep {
    /// A writer on this computer.
    Writer(WriterStep),
    /// One of the user's other computers (or a summary worker), by device id.
    Computer(DeviceId),
}

impl OrderStep {
    /// The local model step.
    pub const LOCAL: Self = Self::Writer(WriterStep::Local);

    /// Whether this is the local model.
    #[must_use]
    pub fn is_local(self) -> bool {
        self == Self::LOCAL
    }

    /// Whether a meeting kept on this computer drops this step (T12.13): an endpoint, Claude Code
    /// or Codex. A computer step stays: that computer writes it with its local model or declines.
    #[must_use]
    pub fn dropped_when_kept_local(self) -> bool {
        matches!(self, Self::Writer(w) if w.is_remote())
    }
}

/// The order: up to [`MAX_STEPS`] steps, or none (no summary).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummaryOrder {
    steps: Vec<OrderStep>,
}

/// What an older build reads: the two old keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mirror {
    /// `settings/summary-writer`: the first writer step, or no summary.
    pub writer: SummaryWriter,
    /// `settings/summary-offload-device`: the computer when it is step 1.
    pub computer: Option<DeviceId>,
}

/// Why an order was refused (nothing stored).
#[derive(Debug)]
pub enum SummaryOrderError {
    /// More than [`MAX_STEPS`] steps.
    TooManySteps,
    /// A writer or computer twice.
    Twice,
    /// This computer as a computer step.
    ThisDevice,
    /// An endpoint step without usable endpoint fields, or another refused setting.
    Settings(SummarySettingsError),
    /// Settings whose endpoint provider differs from the stored one: a caller read them before
    /// the pick changed (T14.72, docs/summary-order.md 12.7a).
    StaleProvider,
    /// An online writer that is not the service picked (12.5).
    NotThePick,
    /// Two online writers (12.5).
    TwoOnline,
    /// Reading or writing the store failed.
    Store(RuntimeError),
}

impl fmt::Display for SummaryOrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManySteps => write!(f, "an order holds at most {MAX_STEPS} steps"),
            Self::Twice => f.write_str("a writer can appear only once in the order"),
            Self::ThisDevice => f.write_str("this computer cannot be one of its own steps"),
            Self::StaleProvider => f.write_str(SETTINGS_CHANGED),
            Self::NotThePick | Self::TwoOnline => f.write_str(NOT_THE_SERVICE),
            Self::Settings(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SummaryOrderError {}

impl From<RuntimeError> for SummaryOrderError {
    fn from(e: RuntimeError) -> Self {
        Self::Store(e)
    }
}

impl From<SummarySettingsError> for SummaryOrderError {
    fn from(e: SummarySettingsError) -> Self {
        match e {
            SummarySettingsError::Store(e) => Self::Store(e),
            e => Self::Settings(e),
        }
    }
}

impl SummaryOrder {
    /// No summary.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// An order of `steps` (empty: no summary), checked for its shape: at most [`MAX_STEPS`],
    /// nothing twice. The endpoint fields and this device are checked by [`set_summary_order`].
    ///
    /// # Errors
    /// [`SummaryOrderError::TooManySteps`], [`SummaryOrderError::Twice`].
    pub fn new(steps: Vec<OrderStep>) -> Result<Self, SummaryOrderError> {
        if steps.len() > MAX_STEPS {
            return Err(SummaryOrderError::TooManySteps);
        }
        for (i, s) in steps.iter().enumerate() {
            if steps[..i].contains(s) {
                return Err(SummaryOrderError::Twice);
            }
        }
        Ok(Self { steps })
    }

    /// The steps, in order (empty: no summary).
    #[must_use]
    pub fn steps(&self) -> &[OrderStep] {
        &self.steps
    }

    /// Whether this is "no summary".
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.steps.is_empty()
    }

    /// The first writer step, if any.
    #[must_use]
    pub fn first_writer(&self) -> Option<WriterStep> {
        self.steps.iter().find_map(|s| match s {
            OrderStep::Writer(w) => Some(*w),
            OrderStep::Computer(_) => None,
        })
    }

    /// Whether the first step is the local model.
    #[must_use]
    pub fn first_is_local(&self) -> bool {
        self.steps.first().is_some_and(|s| s.is_local())
    }

    /// What an older build reads (module docs).
    #[must_use]
    pub fn mirror(&self) -> Mirror {
        Mirror {
            writer: self
                .first_writer()
                .map_or(SummaryWriter::NoSummary, WriterStep::writer),
            computer: match self.steps.first() {
                Some(OrderStep::Computer(d)) => Some(*d),
                _ => None,
            },
        }
    }

    /// The order the old keys say: `[computer]` (if set) then `[writer]`; a writer `none` with a
    /// computer is `[computer]` alone, and `none` with no computer is no summary.
    #[must_use]
    pub fn from_old(writer: SummaryWriter, computer: Option<DeviceId>) -> Self {
        let steps = computer
            .map(OrderStep::Computer)
            .into_iter()
            .chain(WriterStep::of_writer(writer).map(OrderStep::Writer))
            .collect();
        Self { steps }
    }

    /// This order with `device` as its step-1 computer (the Summaries page's "Another computer",
    /// as [`crate::offload::set_chosen_helper`] stores it): `None` removes a step-1 computer;
    /// `Some` replaces it, or is put first, and is removed elsewhere.
    #[must_use]
    pub fn with_helper(&self, device: Option<DeviceId>) -> Self {
        let mut steps: Vec<OrderStep> = self.steps.clone();
        if matches!(steps.first(), Some(OrderStep::Computer(_))) {
            steps.remove(0);
        }
        if let Some(d) = device {
            steps.retain(|s| *s != OrderStep::Computer(d));
            steps.insert(0, OrderStep::Computer(d));
            steps.truncate(MAX_STEPS);
        }
        Self { steps }
    }

    /// The first online writer and its position, if any.
    #[must_use]
    pub fn online_writer(&self) -> Option<(usize, WriterStep)> {
        self.steps.iter().enumerate().find_map(|(i, s)| match s {
            OrderStep::Writer(w) if w.is_online() => Some((i, *w)),
            _ => None,
        })
    }

    /// How many online writers the order holds.
    fn online_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s, OrderStep::Writer(w) if w.is_online()))
            .count()
    }

    /// This order with every online writer after the first dropped, and whether any was
    /// (docs/summary-order.md 12.5).
    #[must_use]
    pub fn normalised(&self) -> (Self, bool) {
        let mut seen = false;
        let mut dropped = false;
        let steps = self
            .steps
            .iter()
            .copied()
            .filter(|s| match s {
                OrderStep::Writer(w) if w.is_online() => {
                    let keep = !seen;
                    seen = true;
                    dropped |= !keep;
                    keep
                }
                _ => true,
            })
            .collect();
        (Self { steps }, dropped)
    }

    /// The steps a run of a meeting with `privacy` uses (docs/summary-order.md 4.2): for a
    /// meeting kept on this computer and a non-empty order, every endpoint, Claude Code and Codex
    /// step is marked dropped and a local step is added when the order holds none. Returns each
    /// step with whether it is dropped; no summary stays no summary.
    #[must_use]
    pub fn for_meeting(&self, privacy: SummaryPrivacy) -> Vec<(OrderStep, bool)> {
        if privacy != SummaryPrivacy::KeptLocal || self.is_none() {
            return self.steps.iter().map(|s| (*s, false)).collect();
        }
        let mut out: Vec<(OrderStep, bool)> = self
            .steps
            .iter()
            .map(|s| (*s, s.dropped_when_kept_local()))
            .collect();
        if !self.steps.iter().any(|s| s.is_local()) {
            out.push((OrderStep::LOCAL, false));
        }
        out
    }

    /// The steps a meeting with `privacy` may try ([`SummaryOrder::for_meeting`] without the
    /// dropped ones).
    #[must_use]
    pub fn filtered(&self, privacy: SummaryPrivacy) -> Vec<OrderStep> {
        self.for_meeting(privacy)
            .into_iter()
            .filter(|(_, dropped)| !dropped)
            .map(|(s, _)| s)
            .collect()
    }
}

/// Whether a meeting whose filtered order is `steps` runs live summaries (docs/summary-order.md
/// 4.5): its first step is the local model, or its first step is a computer and the next is the
/// local model.
#[must_use]
pub fn live_summaries_allowed(steps: &[OrderStep]) -> bool {
    match steps {
        [first, ..] if first.is_local() => true,
        [OrderStep::Computer(_), second, ..] => second.is_local(),
        _ => false,
    }
}

/// Whether any of `steps` (a filtered order) is the local model: the meeting's summary then holds
/// the local model ([`crate::meeting_queue::SummaryHold`], docs/summary-order.md 4.5).
#[must_use]
pub fn any_local(steps: &[OrderStep]) -> bool {
    steps.iter().any(|s| s.is_local())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    steps: Vec<OrderStep>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    none: bool,
}

/// The stored bytes of `order`.
#[must_use]
pub fn encode(order: &SummaryOrder) -> Vec<u8> {
    let stored = Stored {
        steps: order.steps.clone(),
        none: order.is_none(),
    };
    earshot_store::encode_versioned(ORDER_VERSION, &stored).expect("an order serialises")
}

/// The order in `bytes`, `None` when this version cannot read it or it breaks the shape rules.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<SummaryOrder> {
    let (version, payload) = earshot_store::split_version(bytes)?;
    if version != ORDER_VERSION {
        return None;
    }
    let stored: Stored = serde_json::from_slice(payload).ok()?;
    match (stored.none, stored.steps.is_empty()) {
        (true, true) => Some(SummaryOrder::none()),
        (false, false) => SummaryOrder::new(stored.steps).ok(),
        _ => None,
    }
}

/// This install's device id as stored (`None` before the store made one).
fn this_device(store: &Store) -> Option<DeviceId> {
    let bytes = store.local_value("device_id").ok().flatten()?;
    <[u8; 16]>::try_from(bytes.as_slice())
        .ok()
        .map(DeviceId::from_bytes)
}

/// The old keys as stored (text; `None` when absent).
fn old_keys(store: &Store) -> Result<(Option<String>, Option<String>), RuntimeError> {
    let text = |key: &str| -> Result<Option<String>, RuntimeError> {
        Ok(store
            .local_value(key)?
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned()))
    };
    Ok((
        text(SUMMARY_WRITER_KEY)?,
        text(crate::offload::OFFLOAD_DEVICE_KEY)?,
    ))
}

/// Whether the stored old keys are exactly the mirror of an order (an absent device key counts
/// as empty; an absent writer key never matches, because every write stores it).
fn old_keys_match(mirror: Mirror, writer: Option<&str>, device: Option<&str>) -> bool {
    let device_text = mirror.computer.map(|d| d.to_string()).unwrap_or_default();
    writer == Some(mirror.writer.as_str()) && device.unwrap_or("") == device_text
}

/// The order (module docs) and what the user is told about it (fixed copy only).
///
/// # Errors
/// Store errors.
pub fn summary_order(store: &Store) -> Result<(SummaryOrder, Vec<String>), RuntimeError> {
    let (settings, _) = crate::summary_settings::summary_settings(store)?;
    let from_old = || -> Result<SummaryOrder, RuntimeError> {
        Ok(SummaryOrder::from_old(
            settings.writer,
            crate::offload::chosen_helper(store)?,
        ))
    };
    let Some(bytes) = store.local_value(SUMMARY_ORDER_KEY)? else {
        return Ok((from_old()?, Vec::new()));
    };
    let me = this_device(store);
    // At most one online writer (12.5): an older build may have ticked a second. Normalised
    // before the read checks, so a dropped endpoint step never makes the order unreadable.
    let decoded = decode(&bytes).map(|o| o.normalised());
    let dropped = decoded.as_ref().is_some_and(|(_, d)| *d);
    let readable = decoded.map(|(o, _)| o).filter(|order| {
        // What `set_summary_order` refuses is refused on read too: this device as a step, or an
        // endpoint step without a usable endpoint.
        me.is_none_or(|me| !order.steps.contains(&OrderStep::Computer(me)))
            && (!order
                .steps
                .contains(&OrderStep::Writer(WriterStep::Endpoint))
                || settings.has_endpoint())
    });
    let Some(order) = readable else {
        return Ok((from_old()?, vec![UNREADABLE_ORDER.to_owned()]));
    };
    let (writer, device) = old_keys(store)?;
    if old_keys_match(order.mirror(), writer.as_deref(), device.as_deref()) {
        let notes = dropped
            .then(|| SECOND_ONLINE_DROPPED.to_owned())
            .into_iter()
            .collect();
        Ok((order, notes))
    } else {
        // A downgraded build changed the old keys: its choice wins over the stale order.
        Ok((from_old()?, Vec::new()))
    }
}

/// The online service picked, as read (docs/summary-order.md 12.4), and what the user is told
/// about it (fixed copy only): the order's online writer when the order as read holds one (the
/// endpoint's service by the stored provider), else the stored key, else none. A key this build
/// does not know reads as none, with [`UNKNOWN_SERVICE`] (never the stored text).
///
/// # Errors
/// Store errors.
pub fn online_service(store: &Store) -> Result<(Option<Service>, Vec<String>), RuntimeError> {
    let (order, _) = summary_order(store)?;
    let (settings, _) = crate::summary_settings::summary_settings(store)?;
    if let Some((_, w)) = order.online_writer() {
        return Ok((Service::of_step(w, settings.provider), Vec::new()));
    }
    let read = read_setting(store, ONLINE_SERVICE_KEY, |t| {
        WriterStep::parse(t.trim())
            .filter(|w| w.is_online())
            .ok_or(t)
    })?;
    Ok(match read {
        Setting::Known(w) => (Service::of_step(w, settings.provider), Vec::new()),
        Setting::Absent => (None, Vec::new()),
        Setting::Unknown(_) => (None, vec![UNKNOWN_SERVICE.to_owned()]),
    })
}

/// Checks `order` against `settings` (the endpoint fields when an endpoint step is present,
/// wherever it sits) and this device `me` (when known).
///
/// # Errors
/// [`SummaryOrderError`].
pub fn validate(
    order: &SummaryOrder,
    settings: &SummarySettings,
    me: Option<DeviceId>,
) -> Result<(), SummaryOrderError> {
    let order = SummaryOrder::new(order.steps.clone())?;
    if order.online_count() > 1 {
        return Err(SummaryOrderError::TwoOnline);
    }
    if me.is_some_and(|me| order.steps.contains(&OrderStep::Computer(me))) {
        return Err(SummaryOrderError::ThisDevice);
    }
    if order
        .steps
        .contains(&OrderStep::Writer(WriterStep::Endpoint))
    {
        crate::summary_settings::validate(&SummarySettings {
            writer: SummaryWriter::OwnEndpoint,
            ..settings.clone()
        })?;
    }
    Ok(())
}

/// A fault the tests inject between the order and the mirror (the transaction then rolls back).
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    thread_local! {
        static AFTER_ORDER: Cell<bool> = const { Cell::new(false) };
        static AFTER_PROVIDER: Cell<bool> = const { Cell::new(false) };
    }

    /// Fails the next order write on this thread after the order is written.
    pub(crate) fn fail_after_order() {
        AFTER_ORDER.with(|f| f.set(true));
    }

    /// Fails the next order write on this thread that changes the endpoint's provider, right
    /// after the provider is written (T14.72).
    pub(crate) fn fail_after_provider() {
        AFTER_PROVIDER.with(|f| f.set(true));
    }

    pub(super) fn take() -> bool {
        AFTER_ORDER.with(|f| f.replace(false))
    }

    pub(super) fn take_provider() -> bool {
        AFTER_PROVIDER.with(|f| f.replace(false))
    }
}

/// Stores `order` and its mirror in one transaction (module docs), with `settings` (every
/// writer setting, the writer key and the provider aside) when the page changes both; returns
/// what was stored. The endpoint fields are checked against `settings`, or the stored ones
/// without. The pick is stored with it (docs/summary-order.md 12.4).
///
/// # Errors
/// [`SummaryOrderError`]: nothing is stored on a refusal. Settings whose provider differs from
/// the stored one are [`SummaryOrderError::StaleProvider`]; two online writers
/// [`SummaryOrderError::TwoOnline`]; an online writer that is not the pick
/// [`SummaryOrderError::NotThePick`].
pub fn set_summary_order(
    store: &mut Store,
    order: &SummaryOrder,
    settings: Option<&SummarySettings>,
) -> Result<SummaryOrder, SummaryOrderError> {
    write_order(store, order, settings, false, false, None).map(|(o, _)| o)
}

/// [`set_summary_order`] for a change that leaves the writer alone (choosing the helper,
/// [`crate::offload::set_chosen_helper`]): a stored writer word that does not read as itself
/// (a word this build does not know, or "your own endpoint" without a usable endpoint, both read
/// as the default) is kept as stored instead of being replaced by the mirror of that default.
/// The old keys then differ from the mirror, so the order reads from them, as it did before.
///
/// Likewise a stored order that does not read as itself ([`UNREADABLE_ORDER`]: a newer build's
/// format, or one refused on read) is left as stored (T12.63b): only the old keys are written,
/// and the order reads from them, as it already did.
///
/// # Errors
/// As [`set_summary_order`].
pub(crate) fn set_summary_order_keeping_writer(
    store: &mut Store,
    order: &SummaryOrder,
) -> Result<SummaryOrder, SummaryOrderError> {
    let (writer, _) = old_keys(store)?;
    let effective = crate::summary_settings::summary_settings(store)?.0.writer;
    let keep = writer.is_some_and(|w| w != effective.as_str());
    let unreadable = summary_order(store)?
        .1
        .iter()
        .any(|n| n == UNREADABLE_ORDER);
    write_order(store, order, None, keep, unreadable, None).map(|(o, _)| o)
}

/// Stores every Summaries setting with the order as read (T14.72: the settings never change the
/// order or the provider; [`crate::summary_settings::set_summary_settings`]). The endpoint's
/// address and model are the stored ones whatever `settings` say: only the endpoint form's save
/// ([`set_summary_order`] with settings) changes them, so a save from a page read before two
/// pick switches cannot write back an address the 12.7 clear removed. Returns the settings as
/// stored, the writer the order's mirror.
///
/// # Errors
/// As [`set_summary_order`].
pub(crate) fn set_settings_with_order(
    store: &mut Store,
    settings: &SummarySettings,
) -> Result<SummarySettings, SummaryOrderError> {
    let (order, _) = summary_order(store)?;
    let (stored, _) = crate::summary_settings::summary_settings(store)?;
    let settings = SummarySettings {
        url: stored.url,
        model: stored.model,
        ..settings.clone()
    };
    let (_, stored) = write_order(store, &order, Some(&settings), false, false, None)?;
    Ok(stored.unwrap_or(settings))
}

/// Picks `service` as the one online service (docs/summary-order.md 12.6), always writing the
/// order as read: an order holding the old pick gets `service` in its place when it can be a
/// step (Claude Code and Codex always; an endpoint service whose fields pass the rules after the
/// 12.7 clear), else loses that step; an order with no online writer is stored unchanged.
/// Picking the service already picked stores nothing when the stored order already reads as
/// itself; when it does not (unreadable, old keys that win, a second online writer, or nothing
/// stored), the order as read is written, so rule 1 of 12.4 reads the same pick from then on. The
/// reads happen under the caller's `&mut Store` (the services lock) before the write's
/// transaction, which is safe because every writer of these keys holds it. An endpoint service
/// whose provider differs from the stored one stores its provider and clears the address and
/// model in the same transaction (12.7). Returns the order as stored.
///
/// # Errors
/// [`SummaryOrderError`]: nothing is stored on a refusal.
pub fn set_online_service(
    store: &mut Store,
    service: Service,
) -> Result<SummaryOrder, SummaryOrderError> {
    let (order, _) = summary_order(store)?;
    let (read_pick, _) = online_service(store)?;
    if read_pick == Some(service) {
        if stored_order_reads_as(store, &order)? {
            return Ok(order);
        }
        return write_order(store, &order, None, false, false, Some(service)).map(|(o, _)| o);
    }
    let (stored, _) = crate::summary_settings::summary_settings(store)?;
    let after = settings_after_pick(&stored, Some(service));
    let can_step = match service.step() {
        WriterStep::Endpoint => crate::summary_settings::validate(&SummarySettings {
            writer: SummaryWriter::OwnEndpoint,
            ..after
        })
        .is_ok(),
        _ => true,
    };
    let mut steps = order.steps().to_vec();
    if let Some((i, _)) = order.online_writer() {
        if can_step {
            steps[i] = OrderStep::Writer(service.step());
        } else {
            steps.remove(i);
        }
    }
    let new = SummaryOrder::new(steps)?;
    write_order(store, &new, None, false, false, Some(service)).map(|(o, _)| o)
}

/// Whether the stored order is exactly `order` as read: stored, readable, used (the old keys
/// match its mirror) and already in the normal form.
fn stored_order_reads_as(store: &Store, order: &SummaryOrder) -> Result<bool, RuntimeError> {
    let (read, notes) = summary_order(store)?;
    let stored = store
        .local_value(SUMMARY_ORDER_KEY)?
        .and_then(|b| decode(&b));
    Ok(notes.is_empty() && read == *order && stored.as_ref() == Some(order))
}

/// The endpoint settings after picking `pick` (12.7): an endpoint service whose provider differs
/// from the stored one takes its provider with the address and model cleared; anything else
/// leaves them as stored.
fn settings_after_pick(stored: &SummarySettings, pick: Option<Service>) -> SummarySettings {
    match pick.and_then(Service::provider) {
        Some(p) if p != stored.provider => SummarySettings {
            provider: p,
            url: String::new(),
            model: String::new(),
            ..stored.clone()
        },
        _ => stored.clone(),
    }
}

fn write_order(
    store: &mut Store,
    order: &SummaryOrder,
    settings: Option<&SummarySettings>,
    keep_writer: bool,
    keep_order: bool,
    new_pick: Option<Service>,
) -> Result<(SummaryOrder, Option<SummarySettings>), SummaryOrderError> {
    let (stored, _) = crate::summary_settings::summary_settings(store)?;
    let (read_pick, _) = online_service(store)?;
    // The provider has one writer (12.7a): settings read before a pick changed it are refused,
    // before anything else is checked.
    if settings.is_some_and(|s| s.provider != stored.provider) {
        return Err(SummaryOrderError::StaleProvider);
    }
    // At most one online writer, and it is the pick (12.5).
    if order.online_count() > 1 {
        return Err(SummaryOrderError::TwoOnline);
    }
    let pick = new_pick.or(read_pick);
    if let Some((_, w)) = order.online_writer()
        && pick.map(Service::step) != Some(w)
    {
        return Err(SummaryOrderError::NotThePick);
    }
    let checked = match settings {
        Some(s) => Some(crate::summary_settings::validate(&SummarySettings {
            writer: order.mirror().writer,
            ..s.clone()
        })?),
        None => None,
    };
    let provider_change = new_pick
        .and_then(Service::provider)
        .filter(|p| *p != stored.provider);
    let against = settings_after_pick(checked.as_ref().unwrap_or(&stored), new_pick);
    validate(order, &against, this_device(store))?;
    let mirror = order.mirror();
    let bytes = encode(order);
    let device_text = mirror.computer.map(|d| d.to_string()).unwrap_or_default();
    // The key follows the order's online writer, else the new pick, else the pick as read: an
    // untick never changes the pick (12.4).
    let key = order
        .online_writer()
        .map(|(_, w)| w)
        .or(new_pick.map(Service::step))
        .or(read_pick.map(Service::step));
    let values = checked
        .as_ref()
        .map(crate::summary_settings::stored_values)
        .unwrap_or_default();
    store
        .transaction(|tx| -> Result<(), RuntimeError> {
            if !keep_order {
                crate::local::set_tx(tx, &DeviceRecord::SummaryOrder.key(), &bytes)?;
            }
            #[cfg(test)]
            if fault::take() {
                return Err(RuntimeError::Internal("an injected fault"));
            }
            if !keep_writer {
                crate::local::set_tx(
                    tx,
                    &DeviceRecord::SummaryWriter.key(),
                    mirror.writer.as_str().as_bytes(),
                )?;
            }
            crate::local::set_tx(
                tx,
                &DeviceRecord::SummaryOffloadDevice.key(),
                device_text.as_bytes(),
            )?;
            if let Some(w) = key {
                crate::local::set_tx(tx, &DeviceRecord::OnlineService.key(), w.word().as_bytes())?;
            }
            if let Some(p) = provider_change {
                crate::local::set_tx(
                    tx,
                    &DeviceRecord::SummaryProvider.key(),
                    p.as_str().as_bytes(),
                )?;
                #[cfg(test)]
                if fault::take_provider() {
                    return Err(RuntimeError::Internal("an injected fault"));
                }
                crate::local::set_tx(tx, &DeviceRecord::SummaryEndpoint.key(), b"")?;
                crate::local::set_tx(tx, &DeviceRecord::SummaryEndpointModel.key(), b"")?;
            }
            for (record, value) in &values {
                crate::local::set_tx(tx, &record.key(), value.as_bytes())?;
            }
            Ok(())
        })
        .map_err(SummaryOrderError::Store)?;
    Ok((
        order.clone(),
        checked.map(|s| SummarySettings {
            writer: mirror.writer,
            ..s
        }),
    ))
}

#[cfg(test)]
#[path = "summary_order_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "summary_order/online_tests.rs"]
mod online_tests;
