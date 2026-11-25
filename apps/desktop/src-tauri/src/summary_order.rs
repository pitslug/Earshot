//! Summary writers in order in the app (T12.63b; docs/summary-order.md sections 2, 4.4, 4.5, 5
//! and 10; the rules are `earshot_runtime::summary_order`).
//!
//! - The Summaries page's one "Who writes the summary" section edits the order: up to three
//!   steps, or no summary ([`order_view`], [`set_order`]); each step shows where the transcript
//!   goes ([`destination`]).
//! - A meeting's summary runs the order read when the summary is due (the meeting driver's
//!   `after`, through [`SummaryProbes`] and [`with_writers`]); a summary written by a later step,
//!   or none, says why in banner [`STEPS_BANNER_KEY`] ([`steps_text`]); a step known to fail at
//!   the start says so once ([`start_banners`]).
//! - Re-summarise offers "Use my order" and every writer that can run here now
//!   ([`resummarise_view`]); a writer named there is a named run with no next step.
//! - The meeting detail shows the last run's account: interrupted, a named computer's hand-back,
//!   or the steps ([`run_note`]).
//!
//! All copy is fixed, from `fixtures/ui/summary_order_cases.json` ([`COPY`], read by the UI and
//! its mock too). A computer's name is the user's own, from its presence; the endpoint's host is
//! the user's own setting; no meeting content appears here.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use earshot_core::identity::{DeviceId, EntityId};
use earshot_runtime::Services;
use earshot_runtime::backends::ModelDir;
use earshot_runtime::claude_code::ClaudeCodeProbe;
use earshot_runtime::codex::CodexProbe;
use earshot_runtime::host::{self, SettingsWriters};
use earshot_runtime::keep_local::SummaryPrivacy;
use earshot_runtime::offload::{OffloadChoice, WriterKind};
use earshot_runtime::summary_order::run::{
    OutcomeWord, RunOrigin, RunStepView, RunView, StepResult,
};
use earshot_runtime::summary_order::{OrderStep, SummaryOrder, SummaryOrderError, WriterStep};
use earshot_runtime::summary_settings::{ApiKeyStore, EndpointProvider, SummarySettings};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ipc::IpcError;
use crate::meeting_events::{MeetingEvent, WarningKind};
use crate::summaries::SummarySettingsValue;
use crate::sync_settings::SyncHost;

// ---- copy ----

/// One text per kind of step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepTexts {
    /// The local model.
    pub local: String,
    /// The endpoint on Anthropic.
    pub endpoint_anthropic: String,
    /// The endpoint on a model server (`{host}`).
    pub endpoint_server: String,
    /// The endpoint before one is set.
    pub endpoint_unset: String,
    /// Claude through Claude Code.
    pub claude_code: String,
    /// OpenAI through Codex.
    pub codex: String,
    /// A computer (`{device}`, `{writer}`).
    pub computer: String,
    /// A computer whose writer kind is not known (`{device}`).
    pub computer_no_writer: String,
}

/// One banner subject per kind of step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterTexts {
    /// The local model.
    pub local: String,
    /// The endpoint on Anthropic.
    pub endpoint_anthropic: String,
    /// The endpoint on a model server (`{host}`).
    pub endpoint_server: String,
    /// The endpoint before one is set.
    pub endpoint_unset: String,
    /// Claude through Claude Code.
    pub claude_code: String,
    /// OpenAI through Codex.
    pub codex: String,
    /// A computer (`{device}`).
    pub computer: String,
}

/// The Summaries page's texts (the UI reads them from the table; parsed here so the table's shape
/// is pinned on both sides).
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageTexts {
    /// "Write a summary".
    pub write: String,
    /// "No summary".
    pub no_summary: String,
    /// What no summary means.
    pub no_summary_why: String,
    /// The list of writers' accessible name (T12.68).
    pub list: String,
    /// "Step {n}": a ticked writer's rank.
    pub step: String,
    /// A writer's tick box (`{writer}` its label).
    pub r#use: String,
    /// A ticked writer's up button (`{writer}`).
    pub move_up: String,
    /// A ticked writer's down button (`{writer}`).
    pub move_down: String,
    /// A tick refused because the order is full (`{n}` the most steps).
    pub too_many: String,
    /// A writer that cannot be ticked until it is set up.
    pub not_set_up: String,
    /// The local model while no summary model is installed (it cannot be ticked then).
    pub not_downloaded: String,
    /// What the order does.
    pub explained: String,
    /// What a meeting kept on this computer does with the order (under the list).
    pub kept_local: String,
    /// While the order is read.
    pub reading: String,
    /// The order could not be read.
    pub not_read: String,
}

/// The re-summarise button's texts (`order` and `choose` are the UI's).
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResummariseTexts {
    /// "Use my order".
    pub order: String,
    /// Why a remote writer is disabled for a meeting kept on this computer.
    pub kept_local: String,
    /// The menu button's accessible name (`{label}`).
    pub choose: String,
    /// While the menu reads the writers.
    pub looking: String,
    /// A re-summary that wrote nothing.
    pub no_summary_title: String,
    /// A re-summary that asked another computer.
    pub offloaded_title: String,
}

/// The copy table (`fixtures/ui/summary_order_cases.json`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Copy {
    #[serde(rename = "_comment")]
    _comment: String,
    /// Each step's label.
    pub steps: StepTexts,
    /// Where each step sends the transcript.
    pub destinations: StepTexts,
    /// Each step as a banner's subject.
    pub writers: WriterTexts,
    /// Each outcome word, by the runtime's `OutcomeWord` serialised.
    pub words: BTreeMap<String, String>,
    /// A writer and its word (`{writer}`, `{word}`).
    pub step_outcome: String,
    /// Between step outcomes.
    pub joiner: String,
    /// A later step wrote it (`{writer}`, `{steps}`).
    pub later_writer: String,
    /// Every step failed (`{steps}`).
    pub no_summary: String,
    /// A step known to fail at the start, with a later step (`{writer}`, `{word}`, `{next}`).
    pub start_check: String,
    /// The same, with no later step.
    pub start_check_last: String,
    /// A run that did not end.
    pub interrupted: String,
    /// A named computer handed it back (`{device}`, `{word}`).
    pub named_hand_back: String,
    /// A re-summary going on with the next step (`{device}`).
    pub continue_note: String,
    /// A local step skipped while a newer meeting needed this computer.
    pub not_now: String,
    /// The Summaries page (the UI's).
    #[allow(dead_code)]
    pub page: PageTexts,
    /// The re-summarise button.
    pub resummarise: ResummariseTexts,
    /// A refused order, by `SummaryOrderError` (pinned to [`order_error`] by its test; the UI
    /// mock reads them).
    #[allow(dead_code)]
    pub refusals: BTreeMap<String, String>,
    /// A computer whose name is not known.
    pub unnamed: String,
}

/// The copy table, parsed once.
pub static COPY: LazyLock<Copy> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../../../fixtures/ui/summary_order_cases.json"
    ))
    .expect("the summary order copy table parses (pinned by its test)")
});

/// The banner key of a summary written by a later step, or of none.
pub const STEPS_BANNER_KEY: &str = "summary-steps";

/// `template` with each `{name}` replaced (literally: a name holding `$` stays as written).
fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    pairs.iter().fold(template.to_owned(), |text, (k, v)| {
        text.replace(&format!("{{{k}}}"), v)
    })
}

/// The table key of `word` (its serialised name).
#[must_use]
pub fn word_key(word: OutcomeWord) -> String {
    serde_json::to_value(word)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The words the user reads for `word`.
#[must_use]
pub fn word(word: OutcomeWord) -> String {
    COPY.words.get(&word_key(word)).cloned().unwrap_or_default()
}

// ---- naming steps ----

/// What is known of one of the user's other computers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Named {
    /// Its name, from its presence.
    pub name: Option<String>,
    /// Its writer kind, from its presence.
    pub writer: Option<WriterKind>,
    /// Where its own local model runs, for a summary worker whose writer is its local model
    /// (from its presence, T11.6a).
    pub place: Option<earshot_runtime::sync::ModelPlace>,
}

/// What a step's copy needs: the endpoint's provider and host, and the computers' names.
#[derive(Debug, Clone)]
pub struct StepContext {
    /// The endpoint's provider.
    pub provider: EndpointProvider,
    /// The endpoint's host, when one is set.
    pub host: Option<String>,
    /// The computers known now.
    pub names: BTreeMap<DeviceId, Named>,
}

impl StepContext {
    /// The context of `settings` and the computers this process has heard of.
    #[must_use]
    pub fn of(settings: &SummarySettings) -> Self {
        Self {
            provider: settings.provider,
            host: settings
                .has_endpoint()
                .then(|| settings.endpoint_host())
                .flatten(),
            names: crate::offload::known_computers(),
        }
    }

    /// The context of the settings in `services`' store (defaults when they cannot be read).
    #[must_use]
    pub fn read(services: &Services) -> Self {
        let settings = earshot_runtime::summary_settings::summary_settings(&services.store())
            .map(|(s, _)| s)
            .unwrap_or_default();
        Self::of(&settings)
    }

    fn device(&self, device: DeviceId) -> String {
        self.names
            .get(&device)
            .and_then(|n| n.name.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| COPY.unnamed.clone())
    }

    fn writer_kind(&self, device: DeviceId) -> Option<WriterKind> {
        self.names.get(&device).and_then(|n| n.writer)
    }

    fn model_place(&self, device: DeviceId) -> Option<earshot_runtime::sync::ModelPlace> {
        self.names.get(&device).and_then(|n| n.place)
    }
}

fn texts_of(texts: &StepTexts, step: OrderStep, cx: &StepContext) -> String {
    match step {
        OrderStep::Writer(WriterStep::Local) => texts.local.clone(),
        OrderStep::Writer(WriterStep::Endpoint) => match (cx.provider, &cx.host) {
            (_, None) => texts.endpoint_unset.clone(),
            (EndpointProvider::Anthropic, Some(_)) => texts.endpoint_anthropic.clone(),
            (EndpointProvider::OpenAiCompatible, Some(host)) => {
                fill(&texts.endpoint_server, &[("host", host)])
            }
        },
        OrderStep::Writer(WriterStep::ClaudeCode) => texts.claude_code.clone(),
        OrderStep::Writer(WriterStep::Codex) => texts.codex.clone(),
        OrderStep::Computer(d) => match cx.writer_kind(d) {
            Some(k) => fill(
                &texts.computer,
                &[
                    ("device", &cx.device(d)),
                    (
                        "writer",
                        crate::offload::computer_writer_label(k, cx.model_place(d)),
                    ),
                ],
            ),
            None => fill(&texts.computer_no_writer, &[("device", &cx.device(d))]),
        },
    }
}

/// A step's label (the Summaries page and the re-summarise menu).
#[must_use]
pub fn step_label(step: OrderStep, cx: &StepContext) -> String {
    texts_of(&COPY.steps, step, cx)
}

/// Where a step sends the transcript.
#[must_use]
pub fn destination(step: OrderStep, cx: &StepContext) -> String {
    texts_of(&COPY.destinations, step, cx)
}

/// A step as a banner's subject.
#[must_use]
pub fn writer_name(step: OrderStep, cx: &StepContext) -> String {
    let w = &COPY.writers;
    match step {
        OrderStep::Writer(WriterStep::Local) => w.local.clone(),
        OrderStep::Writer(WriterStep::Endpoint) => match (cx.provider, &cx.host) {
            (_, None) => w.endpoint_unset.clone(),
            (EndpointProvider::Anthropic, Some(_)) => w.endpoint_anthropic.clone(),
            (EndpointProvider::OpenAiCompatible, Some(host)) => {
                fill(&w.endpoint_server, &[("host", host)])
            }
        },
        OrderStep::Writer(WriterStep::ClaudeCode) => w.claude_code.clone(),
        OrderStep::Writer(WriterStep::Codex) => w.codex.clone(),
        OrderStep::Computer(d) => fill(&w.computer, &[("device", &cx.device(d))]),
    }
}

// ---- banners ----

/// How many steps of a run a meeting could use: every step but those dropped for a meeting kept
/// on this computer. A run of one such step is a one-step order (or a named run): today's
/// banners and refusals say what became of it (docs/summary-order.md 4.4).
#[must_use]
pub fn tried_steps(views: &[RunStepView]) -> usize {
    views
        .iter()
        .filter(|v| v.result != StepResult::Moved(OutcomeWord::SkippedKeptLocal))
        .count()
}

/// The `summary-steps` text of a run with `views` (docs/summary-order.md 4.4): `None` for a
/// one-step run (today's banners say it), a summary written by step 1, a run still waiting or a
/// run that stopped before every step was tried.
#[must_use]
pub fn steps_text(views: &[RunStepView], cx: &StepContext) -> Option<String> {
    if tried_steps(views) < 2 {
        return None;
    }
    let moved = |upto: usize| -> String {
        views[..upto]
            .iter()
            .filter_map(|v| match v.result {
                StepResult::Moved(w) => Some(fill(
                    &COPY.step_outcome,
                    &[("writer", &writer_name(v.step, cx)), ("word", &word(w))],
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(&COPY.joiner)
    };
    match views.iter().position(|v| v.result == StepResult::Wrote) {
        Some(0) => None,
        Some(i) => {
            let steps = moved(i);
            (!steps.is_empty()).then(|| {
                fill(
                    &COPY.later_writer,
                    &[
                        ("writer", &writer_name(views[i].step, cx)),
                        ("steps", &steps),
                    ],
                )
            })
        }
        // Every step was tried and left: no summary.
        None if views
            .iter()
            .all(|v| matches!(v.result, StepResult::Moved(_))) =>
        {
            Some(fill(&COPY.no_summary, &[("steps", &moved(views.len()))]))
        }
        None => None,
    }
}

/// The `summary-steps` banner of a run with `views`, when [`steps_text`] has one.
#[must_use]
pub fn steps_banner(views: &[RunStepView], cx: &StepContext) -> Option<MeetingEvent> {
    steps_text(views, cx).map(|message| MeetingEvent::Warning {
        key: STEPS_BANNER_KEY.into(),
        kind: WarningKind::Backend,
        message,
    })
}

/// The meeting-start banners of the steps known to fail now (docs/summary-order.md 4.5): one per
/// failing step of a run of two or more (a one-step order keeps today's banners), keys
/// `summary-start-<i>`. They never block recording; every step is still tried.
#[must_use]
pub fn start_banners(
    steps: &[OrderStep],
    failing: &[(usize, OutcomeWord)],
    cx: &StepContext,
) -> Vec<MeetingEvent> {
    if steps.len() < 2 {
        return Vec::new();
    }
    failing
        .iter()
        .map(|(i, w)| {
            let next = (i + 1..steps.len()).find(|j| !failing.iter().any(|(f, _)| f == j));
            let writer = writer_name(steps[*i], cx);
            let word = word(*w);
            let message = match next {
                Some(j) => fill(
                    &COPY.start_check,
                    &[
                        ("writer", &writer),
                        ("word", &word),
                        ("next", &writer_name(steps[j], cx)),
                    ],
                ),
                None => fill(
                    &COPY.start_check_last,
                    &[("writer", &writer), ("word", &word)],
                ),
            };
            MeetingEvent::Warning {
                key: format!("summary-start-{i}"),
                kind: WarningKind::Backend,
                message,
            }
        })
        .collect()
}

/// The writer steps of `steps` known to fail now, with their words (the meeting start's checks;
/// docs/summary-order.md 4.5). Computer steps are not asked at the start: asking the relay would
/// hold up the recording, and a computer's state at the start says little about it at the end.
#[must_use]
pub fn writer_start_checks(
    services: &Services,
    steps: &[OrderStep],
    writers: &dyn earshot_runtime::summary_order::run::StepWriters,
) -> Vec<(usize, OutcomeWord)> {
    use earshot_runtime::summary_order::run::{Built, skip_word};
    steps
        .iter()
        .enumerate()
        .filter_map(|(i, step)| match step {
            OrderStep::Writer(w) => match writers.writer(services, *w) {
                Built::Service(_) => None,
                Built::Skip(reason) => Some((i, skip_word(&reason).unwrap_or(OutcomeWord::Failed))),
                Built::Stop(_) => Some((i, OutcomeWord::Failed)),
            },
            OrderStep::Computer(_) => None,
        })
        .collect()
}

/// A probe asked once: the meeting start's checks and its summary setup share one answer.
pub struct Once<'a, P: ?Sized, S> {
    probe: &'a P,
    answer: std::cell::OnceCell<S>,
}

impl<'a, P: ?Sized, S> Once<'a, P, S> {
    /// `probe`, asked at most once.
    #[must_use]
    pub fn new(probe: &'a P) -> Self {
        Self {
            probe,
            answer: std::cell::OnceCell::new(),
        }
    }
}

impl ClaudeCodeProbe
    for Once<'_, dyn ClaudeCodeProbe + '_, earshot_runtime::claude_code::ClaudeCodeStatus>
{
    fn status(&self) -> earshot_runtime::claude_code::ClaudeCodeStatus {
        self.answer.get_or_init(|| self.probe.status()).clone()
    }
}

impl CodexProbe for Once<'_, dyn CodexProbe + '_, earshot_runtime::codex::CodexStatus> {
    fn status(&self) -> earshot_runtime::codex::CodexStatus {
        self.answer.get_or_init(|| self.probe.status()).clone()
    }
}

/// A named computer that handed its request back.
#[must_use]
pub fn named_hand_back(device: DeviceId, w: OutcomeWord, cx: &StepContext) -> String {
    fill(
        &COPY.named_hand_back,
        &[("device", &cx.device(device)), ("word", &word(w))],
    )
}

/// The first warning of a re-summary going on with the next step after `device` handed it back.
#[must_use]
pub fn continue_note(device: DeviceId, cx: &StepContext) -> String {
    fill(&COPY.continue_note, &[("device", &cx.device(device))])
}

/// The meeting detail's account of the last run (docs/summary-order.md 4.3, 4.4): interrupted
/// (a run that did not end while nothing works on the meeting, or a record that cannot be read),
/// a named computer's hand-back, or the steps; `None` otherwise.
#[must_use]
pub fn run_note(view: &RunView, working: bool, cx: &StepContext) -> Option<String> {
    match view {
        RunView::Unreadable => Some(COPY.interrupted.clone()),
        RunView::Unfinished(_) if !working => Some(COPY.interrupted.clone()),
        RunView::Ended(run) => {
            if run.origin == RunOrigin::Named
                && run.writer.is_none()
                && let [OrderStep::Computer(d)] = run.steps[..]
                && let Some(w) = run.outcome_of(0)
            {
                return Some(named_hand_back(d, w, cx));
            }
            steps_text(&earshot_runtime::summary_order::run::views(run), cx)
        }
        _ => None,
    }
}

// ---- the writers of a run ----

/// What a summary step reads when it is tried: the Anthropic key store and the two agent CLIs.
#[derive(Clone)]
pub struct SummaryProbes {
    /// The Anthropic key store.
    pub keys: Arc<dyn ApiKeyStore + Send + Sync>,
    /// Claude Code.
    pub claude: Arc<dyn ClaudeCodeProbe + Send + Sync>,
    /// Codex.
    pub codex: Arc<dyn CodexProbe + Send + Sync>,
}

impl SummaryProbes {
    /// The installed ones: Credential Manager, and the CLIs with the meeting start's deadline.
    #[must_use]
    pub fn installed() -> Self {
        Self {
            keys: Arc::new(crate::credentials::CredentialManager::anthropic()),
            claude: Arc::new(earshot_runtime::claude_code::InstalledClaudeCode::meeting_start()),
            codex: Arc::new(earshot_runtime::codex::InstalledCodex::meeting_start()),
        }
    }
}

/// Decides whether to ask one of the user's computers now (the app:
/// `offload::choose_device_now` once sync is ready).
pub type ComputerChooser = Arc<dyn Fn(DeviceId) -> OffloadChoice + Send + Sync>;

/// Where the app puts its [`ComputerChooser`] once sync is ready (shared by the meeting driver's
/// pre-flight, the jobs and the offload host).
pub type ComputerSlot = Arc<std::sync::OnceLock<ComputerChooser>>;

/// Runs `f` with the production writers of a run (`host::SettingsWriters`) over `models` and
/// `probes`; each computer step from `computers` (none: every computer reads as sync off).
pub fn with_writers<R>(
    models: &ModelDir,
    keys: &dyn ApiKeyStore,
    claude: &dyn ClaudeCodeProbe,
    codex: &dyn CodexProbe,
    computers: Option<&dyn Fn(DeviceId) -> OffloadChoice>,
    f: impl FnOnce(&SettingsWriters<'_>) -> R,
) -> R {
    let hardware = earshot_runtime::hardware();
    let writers = SettingsWriters {
        models,
        keys,
        claude,
        codex,
        dll_dirs: host::dll_search_dirs(),
        order: earshot_runtime::backends::accelerator_order(),
        hardware: &hardware,
        computers,
    };
    f(&writers)
}

/// The chooser in `slot`, remembering each computer's name and writer kind for the copy.
#[must_use]
pub fn chooser_of(slot: &ComputerSlot) -> Option<ComputerChooser> {
    slot.get().cloned()
}

/// `choose` as a borrowed `Fn`, remembering what it answers about each computer.
pub fn remembering(choose: &ComputerChooser, device: DeviceId) -> OffloadChoice {
    let choice = choose(device);
    match &choice {
        OffloadChoice::Ask { name, writer, .. } => crate::offload::remember_computer(
            device,
            Some(name.clone()).filter(|n| !n.is_empty()),
            Some(*writer),
        ),
        OffloadChoice::HereInstead { name, .. } => {
            crate::offload::remember_computer(device, name.clone(), None);
        }
        OffloadChoice::Here => {}
    }
    choice
}

// ---- the Summaries page ----

/// One step as the page sends and reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind")]
pub enum StepChoice {
    /// The local model.
    Local,
    /// The user's one endpoint.
    Endpoint,
    /// Claude through Claude Code.
    ClaudeCode,
    /// OpenAI through Codex.
    Codex,
    /// One of the user's other computers.
    Computer {
        /// Its device id.
        device: String,
    },
}

impl StepChoice {
    /// The choice of `step`.
    #[must_use]
    pub fn of(step: OrderStep) -> Self {
        match step {
            OrderStep::Writer(WriterStep::Local) => Self::Local,
            OrderStep::Writer(WriterStep::Endpoint) => Self::Endpoint,
            OrderStep::Writer(WriterStep::ClaudeCode) => Self::ClaudeCode,
            OrderStep::Writer(WriterStep::Codex) => Self::Codex,
            OrderStep::Computer(d) => Self::Computer {
                device: d.to_string(),
            },
        }
    }

    /// The step of this choice.
    ///
    /// # Errors
    /// [`IpcError::invalid`] for a device id that is not one.
    pub fn step(&self) -> Result<OrderStep, IpcError> {
        Ok(match self {
            Self::Local => OrderStep::LOCAL,
            Self::Endpoint => OrderStep::Writer(WriterStep::Endpoint),
            Self::ClaudeCode => OrderStep::Writer(WriterStep::ClaudeCode),
            Self::Codex => OrderStep::Writer(WriterStep::Codex),
            Self::Computer { device } => OrderStep::Computer(crate::offload::device_id(device)?),
        })
    }
}

/// One step with its copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct StepView {
    /// The step.
    pub step: StepChoice,
    /// Its label.
    pub label: String,
    /// Where it sends the transcript.
    pub destination: String,
}

/// The order as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryOrderView {
    /// No summary.
    pub none: bool,
    /// The steps in order (empty with `none`).
    pub steps: Vec<StepView>,
    /// Every step the page can offer: the four writers, then each computer that writes
    /// summaries for the others, or is in the order.
    pub offered: Vec<StepView>,
    /// What the user is told about the stored order (fixed copy).
    pub notes: Vec<String>,
    /// The most steps an order holds.
    pub max_steps: u32,
}

fn step_view(step: OrderStep, cx: &StepContext) -> StepView {
    StepView {
        step: StepChoice::of(step),
        label: step_label(step, cx),
        destination: destination(step, cx),
    }
}

/// The order of `services`' store as the page shows it (asks the server for the computers when
/// sync is on).
///
/// # Errors
/// Store errors.
pub fn order_view(services: &Services, host: &SyncHost) -> Result<SummaryOrderView, IpcError> {
    let (order, notes) = earshot_runtime::summary_order::summary_order(&services.store())?;
    // Reading the computers also remembers their names and writer kinds.
    let helpers = crate::offload::helpers_view(services, host)?;
    let cx = StepContext::read(services);
    let mut offered: Vec<OrderStep> = WriterStep::ALL.into_iter().map(OrderStep::Writer).collect();
    for h in &helpers.helpers {
        if let Ok(d) = crate::offload::device_id(&h.id) {
            offered.push(OrderStep::Computer(d));
        }
    }
    for s in order.steps() {
        if !offered.contains(s) {
            offered.push(*s);
        }
    }
    Ok(SummaryOrderView {
        none: order.is_none(),
        steps: order.steps().iter().map(|s| step_view(*s, &cx)).collect(),
        offered: offered.into_iter().map(|s| step_view(s, &cx)).collect(),
        notes,
        max_steps: u32::try_from(earshot_runtime::summary_order::MAX_STEPS).unwrap_or(u32::MAX),
    })
}

/// The UI error of a refused order.
#[must_use]
pub fn order_error(e: SummaryOrderError) -> IpcError {
    match e {
        SummaryOrderError::Store(e) => IpcError::from(e),
        SummaryOrderError::Settings(e) => crate::summaries::settings_error(e),
        other => IpcError::invalid(crate::summaries::sentence(&other.to_string())),
    }
}

/// Stores the order of `steps` (empty: no summary), with `settings` when the page changes both
/// (the endpoint form under a step); returns the order as stored.
///
/// # Errors
/// `invalid_setting` (nothing stored): too many steps, a step twice, this computer, an endpoint
/// step without usable endpoint fields; store errors.
pub fn set_order(
    services: &Services,
    host: &SyncHost,
    steps: &[StepChoice],
    settings: Option<&SummarySettingsValue>,
) -> Result<SummaryOrderView, IpcError> {
    let steps = steps
        .iter()
        .map(StepChoice::step)
        .collect::<Result<Vec<_>, _>>()?;
    let order = SummaryOrder::new(steps).map_err(order_error)?;
    let settings = settings.map(SummarySettingsValue::settings);
    earshot_runtime::summary_order::set_summary_order(
        &mut services.store(),
        &order,
        settings.as_ref(),
    )
    .map_err(order_error)?;
    order_view(services, host)
}

/// The Summaries page's order (asks the server for the computers).
#[tauri::command]
#[specta::specta]
pub async fn summary_order(
    state: tauri::State<'_, crate::ipc::AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<SummaryOrderView, IpcError> {
    let services = state.services()?;
    let host = Arc::clone(&sync);
    crate::ipc::blocking(move || order_view(&services, &host)).await
}

/// Stores the order of summary writers (empty: no summary), with the endpoint settings when the
/// page changes both.
#[tauri::command]
#[specta::specta]
pub async fn set_summary_order(
    state: tauri::State<'_, crate::ipc::AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    steps: Vec<StepChoice>,
    settings: Option<SummarySettingsValue>,
) -> Result<SummaryOrderView, IpcError> {
    let services = state.services()?;
    let host = Arc::clone(&sync);
    crate::ipc::blocking(move || set_order(&services, &host, &steps, settings.as_ref())).await
}

// ---- re-summarise ----

/// One writer the re-summarise menu offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ResummariseItem {
    /// The writer.
    pub step: StepChoice,
    /// Its label.
    pub label: String,
    /// Why it cannot be chosen (a meeting kept on this computer); `null` when it can.
    pub disabled: Option<String>,
}

/// The re-summarise menu's writers on this computer (computers come from the helpers list).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ResummariseView {
    /// Every writer that can run here now, labelled as on the Summaries page.
    pub writers: Vec<ResummariseItem>,
}

/// What can run on this computer now, for the re-summarise menu (production:
/// [`InstalledWriters`]).
pub trait WritersHere {
    /// The local summary model is installed.
    fn local_installed(&self, services: &Services) -> bool;
    /// An Anthropic API key is saved.
    fn key_saved(&self) -> bool;
    /// Claude Code is installed.
    fn claude_installed(&self) -> bool;
    /// Codex is installed.
    fn codex_installed(&self) -> bool;
}

/// The production [`WritersHere`]: the models folder, Credential Manager (asked whether a key
/// is saved, never read) and the CLIs found where they are installed (nothing is run).
pub struct InstalledWriters;

impl WritersHere for InstalledWriters {
    fn local_installed(&self, services: &Services) -> bool {
        local_installed(services, &host::model_dir(&services.data))
    }

    fn key_saved(&self) -> bool {
        use crate::credentials::KeyVault;
        crate::credentials::CredentialManager::anthropic()
            .has()
            .unwrap_or(false)
    }

    fn claude_installed(&self) -> bool {
        earshot_runtime::claude_code::ClaudeCli::find().is_some()
    }

    fn codex_installed(&self) -> bool {
        earshot_runtime::codex::CodexCli::find().is_some()
    }
}

/// Whether the local summary model the settings choose is installed in `models`.
#[must_use]
pub fn local_installed(services: &Services, models: &ModelDir) -> bool {
    let settings = {
        let db = services.store();
        host::summary_for_writer(
            &db,
            earshot_runtime::summary_settings::SummaryWriter::OnThisComputer,
            &earshot_runtime::backends::accelerator_order(),
            &earshot_runtime::hardware(),
            &earshot_runtime::summary_settings::NoApiKeys,
            SummaryPrivacy::Writer,
        )
    };
    settings.is_ok_and(|s| {
        matches!(
            host::summary_plan(
                &s.request,
                earshot_store::total_memory(),
                s.placement.gpu,
                models
            ),
            Ok(host::SummaryPlan::Local(_))
        )
    })
}

/// The re-summarise menu's writers for `meeting` (docs/summary-order.md 5): the local model if
/// installed; the endpoint if set (with a saved key when it needs one); Claude Code and Codex when
/// installed. For a meeting kept on this computer the remote ones are disabled with the reason
/// until "Allow".
///
/// # Errors
/// Store errors.
pub fn resummarise_view(
    services: &Services,
    meeting: EntityId,
    here: &dyn WritersHere,
) -> Result<ResummariseView, IpcError> {
    let (settings, _) = earshot_runtime::summary_settings::summary_settings(&services.store())?;
    let kept = earshot_runtime::keep_local::summary_privacy(&services.store(), meeting)
        == SummaryPrivacy::KeptLocal;
    let cx = StepContext::of(&settings);
    let mut steps = Vec::new();
    if here.local_installed(services) {
        steps.push(WriterStep::Local);
    }
    // An Anthropic key counts only for the address it was saved for (T14.31), as on the
    // Summaries page and in the run itself.
    let key_usable = || -> Result<bool, IpcError> {
        Ok(here.key_saved()
            && earshot_runtime::summary_settings::api_key_bound(&services.store(), &settings)?)
    };
    if settings.has_endpoint()
        && (settings.provider != EndpointProvider::Anthropic || key_usable()?)
    {
        steps.push(WriterStep::Endpoint);
    }
    if here.claude_installed() {
        steps.push(WriterStep::ClaudeCode);
    }
    if here.codex_installed() {
        steps.push(WriterStep::Codex);
    }
    Ok(ResummariseView {
        writers: steps
            .into_iter()
            .map(|w| ResummariseItem {
                step: StepChoice::of(OrderStep::Writer(w)),
                label: step_label(OrderStep::Writer(w), &cx),
                disabled: (kept && w.is_remote()).then(|| COPY.resummarise.kept_local.clone()),
            })
            .collect(),
    })
}

/// The re-summarise menu's writers for a meeting.
#[tauri::command]
#[specta::specta]
pub async fn resummarise_writers(
    state: tauri::State<'_, crate::ipc::AppState>,
    meeting: String,
) -> Result<ResummariseView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || {
        resummarise_view(
            &services,
            crate::detail::parse_id(&meeting)?,
            &InstalledWriters,
        )
    })
    .await
}

/// Starts a re-summary of `meeting` by the one writer `step` names (a named run: no next step).
/// A computer is asked through `ask_summary_helper`.
#[tauri::command]
#[specta::specta]
pub fn resummarise_with(
    state: tauri::State<'_, crate::ipc::AppState>,
    jobs: tauri::State<'_, crate::jobs::JobRunner>,
    meeting: String,
    step: StepChoice,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let step = match step.step()? {
        OrderStep::Writer(w) => OrderStep::Writer(w),
        OrderStep::Computer(_) => return Err(IpcError::internal()),
    };
    jobs.start_summary(
        &services,
        crate::detail::parse_id(&meeting)?,
        crate::jobs::SummaryStart::Named(step),
        None,
    )
}

#[cfg(test)]
#[path = "summary_order_tests.rs"]
mod tests;
