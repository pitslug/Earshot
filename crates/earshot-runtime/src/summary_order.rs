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
//!   [`crate::summary_settings::set_summary_settings`] and [`crate::offload::set_chosen_helper`]
//!   go through it ([`SummaryOrder::with_writer`], [`SummaryOrder::with_helper`]).
//! - **Read** only through [`summary_order`]: with no stored order, or old keys that differ from
//!   the mirror of the stored order (a downgraded build changed them), the old keys win
//!   ([`SummaryOrder::from_old`]); an unreadable or refused stored order reads as the old keys
//!   with a note ([`UNREADABLE_ORDER`]), never the stored text.
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
use crate::summary_settings::{
    SUMMARY_WRITER_KEY, SummarySettings, SummarySettingsError, SummaryWriter,
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
    /// Reading or writing the store failed.
    Store(RuntimeError),
}

impl fmt::Display for SummaryOrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManySteps => write!(f, "an order holds at most {MAX_STEPS} steps"),
            Self::Twice => f.write_str("a writer can appear only once in the order"),
            Self::ThisDevice => f.write_str("this computer cannot be one of its own steps"),
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

    /// Whether the first step is the local model (the benchmark's rule, docs/summary-order.md
    /// 4.5).
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

    /// This order with `writer` as its first writer step (the Summaries page's one writer, as
    /// [`crate::summary_settings::set_summary_settings`] stores it): no summary removes every
    /// writer step and keeps the computers; any other writer replaces the first writer step (or
    /// is added) and is removed elsewhere.
    #[must_use]
    pub fn with_writer(&self, writer: SummaryWriter) -> Self {
        let Some(w) = WriterStep::of_writer(writer) else {
            return Self {
                steps: self
                    .steps
                    .iter()
                    .copied()
                    .filter(|s| matches!(s, OrderStep::Computer(_)))
                    .collect(),
            };
        };
        let new = OrderStep::Writer(w);
        let mut steps: Vec<OrderStep> = Vec::with_capacity(MAX_STEPS);
        let mut placed = false;
        for s in &self.steps {
            match s {
                OrderStep::Writer(_) if !placed => {
                    steps.push(new);
                    placed = true;
                }
                s if *s == new => {}
                s => steps.push(*s),
            }
        }
        if !placed {
            if steps.len() >= MAX_STEPS {
                steps.truncate(MAX_STEPS - 1);
            }
            steps.push(new);
        }
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
    let readable = decode(&bytes).filter(|order| {
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
        Ok((order, Vec::new()))
    } else {
        // A downgraded build changed the old keys: its choice wins over the stale order.
        Ok((from_old()?, Vec::new()))
    }
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
    }

    /// Fails the next order write on this thread after the order is written.
    pub(crate) fn fail_after_order() {
        AFTER_ORDER.with(|f| f.set(true));
    }

    pub(super) fn take() -> bool {
        AFTER_ORDER.with(|f| f.replace(false))
    }
}

/// Stores `order` and its mirror in one transaction (module docs), with `settings` (every
/// writer setting, the writer key aside) when the page changes both; returns what was stored.
/// The endpoint fields are checked against `settings`, or the stored ones without.
///
/// # Errors
/// [`SummaryOrderError`]: nothing is stored on a refusal.
pub fn set_summary_order(
    store: &mut Store,
    order: &SummaryOrder,
    settings: Option<&SummarySettings>,
) -> Result<SummaryOrder, SummaryOrderError> {
    let checked = match settings {
        Some(s) => Some(crate::summary_settings::validate(&SummarySettings {
            writer: order.mirror().writer,
            ..s.clone()
        })?),
        None => None,
    };
    let against = match &checked {
        Some(s) => s.clone(),
        None => crate::summary_settings::summary_settings(store)?.0,
    };
    validate(order, &against, this_device(store))?;
    let mirror = order.mirror();
    let bytes = encode(order);
    let device_text = mirror.computer.map(|d| d.to_string()).unwrap_or_default();
    let values = checked
        .as_ref()
        .map(crate::summary_settings::stored_values)
        .unwrap_or_default();
    store
        .transaction(|tx| -> Result<(), RuntimeError> {
            crate::local::set_tx(tx, &DeviceRecord::SummaryOrder.key(), &bytes)?;
            #[cfg(test)]
            if fault::take() {
                return Err(RuntimeError::Internal("an injected fault"));
            }
            crate::local::set_tx(
                tx,
                &DeviceRecord::SummaryWriter.key(),
                mirror.writer.as_str().as_bytes(),
            )?;
            crate::local::set_tx(
                tx,
                &DeviceRecord::SummaryOffloadDevice.key(),
                device_text.as_bytes(),
            )?;
            for (record, value) in &values {
                crate::local::set_tx(tx, &record.key(), value.as_bytes())?;
            }
            Ok(())
        })
        .map_err(SummaryOrderError::Store)?;
    Ok(order.clone())
}

#[cfg(test)]
#[path = "summary_order_tests.rs"]
mod tests;
