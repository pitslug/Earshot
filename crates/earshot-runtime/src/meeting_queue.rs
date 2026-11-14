//! The queue of meetings (T9.1l): recording starts at once, the heavy work takes turns.
//!
//! A host takes one [`QueueTicket`] per meeting as the meeting starts, in start order. A meeting's
//! final pass and summary run only when every older ticket has ended ([`QueueTicket::wait_turn`]),
//! so older jobs run first-in, first-out and one summary model runs at a time. Live speaker work
//! waits only for older final passes ([`QueueTicket::wait_final_passes`]), not for a summary, so a
//! long summary does not push the meeting past `SPEAKER_MAX_BUFFER`.
//!
//! Live decoding (T12.39) waits for older final passes and then for an older meeting's summary only
//! when the two would share a device ([`may_run_beside`], the rule in docs/live-previews.md): each
//! ticket records what its summary will hold ([`QueueTicket::set_summary`]), and a meeting whose
//! [`MeetingHold`] knows what its live work needs ([`MeetingHold::beside`]) waits only for those.
//! A ticket that records nothing, or a hold that knows nothing, waits as before: for every older
//! ticket to end.
//!
//! Recording takes no turn: capture and the FLAC writers never wait for this queue.
//! [`MeetingHold`] is the production [`HoldGates`]: a ticket plus the CUDA start, which decides
//! which ONNX Runtime library loads.
//!
//! Meeting jobs (a re-transcription, a re-summary) stay outside the queue, as they ran beside
//! meetings before.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_hardware::Hardware;

use crate::backends::Accelerator;
use crate::host::PreparedSummary;
use crate::live::{HoldGates, HoldReason};

/// How often a wait looks at its cancel token and the CUDA start.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// The least reported physical memory with which a newer meeting's live decoding runs beside an
/// older meeting's summary on this computer (T12.39, docs/live-previews.md). A judgement: a meeting
/// on the CPU or the NPU peaks under 6 GiB of private memory, and a 16 GB machine summarises on the
/// CPU with the 4B, whose 2.6 GiB file is mapped whole with a context cache of about 1 GiB beside
/// it; with the call app, a browser and Windows, a 16 GB machine has room and must pass (it reports
/// under 16 GiB), an 8 GB one has not.
pub const BESIDE_MIN_MEMORY: u64 = 14 * 1024 * 1024 * 1024;

/// The pairs (live transcription's device, summary's device) that may run together on this kind of
/// machine, each switched on only after its measurement passed the rule in docs/live-previews.md
/// (T12.39). A summary written off this computer needs no pair; two work on the CPU never run
/// together whatever is listed, and two on the graphics card only with room for both.
///
/// Empty on x64: measured 2026-11-14 on the desktop (docs/live-previews.md), CUDA transcription
/// beside the card's summary failed lag, summary time and memory, and CPU transcription beside it
/// failed the memory margin only.
#[cfg(target_arch = "x86_64")]
pub const BESIDE_PAIRS: &[(Device, Device)] = &[];

/// As on x64: none until the Surface measures (docs/live-previews.md, T12.39).
#[cfg(not(target_arch = "x86_64"))]
pub const BESIDE_PAIRS: &[(Device, Device)] = &[];

/// A part of this computer that a meeting's work occupies (T12.39).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// The processor.
    Cpu,
    /// The graphics card (CUDA or Vulkan: the machine's discrete card).
    GraphicsCard,
    /// The NPU (QNN).
    Npu,
}

impl Device {
    /// Where work on `accelerator` runs.
    #[must_use]
    pub fn of(accelerator: Accelerator) -> Self {
        match accelerator {
            Accelerator::Cpu => Self::Cpu,
            Accelerator::Cuda | Accelerator::Vulkan => Self::GraphicsCard,
            Accelerator::Qnn => Self::Npu,
        }
    }
}

/// What a meeting's summary will hold on this computer (T12.39).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryHold {
    /// Not recorded: holds every newer meeting's live decoding until the ticket ends, as before.
    Unknown,
    /// No model on this computer: no summary, or one written by Claude Code, Codex or an endpoint.
    OffThisComputer,
    /// A summary model on this computer.
    Local {
        /// Where it runs.
        device: Device,
        /// Its file's size in bytes.
        size: u64,
    },
}

impl SummaryHold {
    /// What `summary` (the meeting's prepared summary, the one its summary step runs) will hold:
    /// a local model on the graphics card when it asks for it, else on the processor.
    #[must_use]
    pub fn of(summary: Option<&PreparedSummary>) -> Self {
        match summary.and_then(|s| s.local.as_ref()) {
            None => Self::OffThisComputer,
            Some((choice, gpu)) => Self::Local {
                device: if *gpu {
                    Device::GraphicsCard
                } else {
                    Device::Cpu
                },
                size: choice.model.size,
            },
        }
    }
}

/// What a newer meeting's live work needs (T12.39).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveNeeds {
    /// Where its live transcription runs (the first accelerator of its order).
    pub device: Device,
    /// The transcription model's file size in bytes.
    pub model_size: u64,
    /// It summarises during the meeting (live summaries: always a local model).
    pub live_summaries: bool,
}

/// The facts of this computer the rule reads.
#[derive(Debug, Clone)]
pub struct Machine {
    /// Its hardware: reported physical memory and the graphics card's.
    pub hardware: Hardware,
    /// The device pairs measured and switched on for its kind ([`BESIDE_PAIRS`]).
    pub pairs: Vec<(Device, Device)>,
}

impl Machine {
    /// This computer, with its kind's switched-on pairs.
    #[must_use]
    pub fn this_one() -> Self {
        Self {
            hardware: crate::hardware(),
            pairs: BESIDE_PAIRS.to_vec(),
        }
    }
}

/// Whether a newer meeting's live work needing `needs` may run beside an older meeting's summary
/// holding `summary` on `machine`: the rule of docs/live-previews.md (T12.39). A summary off this
/// computer never holds; an unrecorded one always does; a local one only lets live work through
/// on a switched-on pair, with system memory for both ([`BESIDE_MIN_MEMORY`]), on another device or
/// on the same graphics card with room for both models ([`fits_graphics_card`]), and never beside
/// live summaries (one summary model at a time).
///
/// [`fits_graphics_card`]: crate::model_settings::fits_graphics_card
#[must_use]
pub fn may_run_beside(summary: SummaryHold, needs: &LiveNeeds, machine: &Machine) -> bool {
    let (device, size) = match summary {
        SummaryHold::Unknown => return false,
        SummaryHold::OffThisComputer => return true,
        SummaryHold::Local { device, size } => (device, size),
    };
    if needs.live_summaries || !machine.pairs.contains(&(needs.device, device)) {
        return false;
    }
    if machine
        .hardware
        .total_memory
        .is_none_or(|m| m < BESIDE_MIN_MEMORY)
    {
        return false;
    }
    if needs.device != device {
        return true;
    }
    device == Device::GraphicsCard
        && crate::model_settings::fits_graphics_card(
            size.saturating_add(needs.model_size),
            &machine.hardware,
        )
}

/// A ticket not yet ended.
#[derive(Debug, Clone, Copy)]
struct Open {
    final_pass_done: bool,
    summary: SummaryHold,
}

#[derive(Debug, Default)]
struct Tickets {
    next: u64,
    /// The tickets not yet ended.
    open: BTreeMap<u64, Open>,
}

#[derive(Debug, Default)]
struct Shared {
    tickets: Mutex<Tickets>,
    changed: Condvar,
}

impl Shared {
    fn tickets(&self) -> std::sync::MutexGuard<'_, Tickets> {
        self.tickets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The queue (cheap to clone; clones share it).
#[derive(Debug, Clone, Default)]
pub struct MeetingQueue(Arc<Shared>);

impl MeetingQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The next meeting's place: behind every ticket not yet ended. Its summary is
    /// [`SummaryHold::Unknown`] until [`QueueTicket::set_summary`].
    #[must_use]
    pub fn enter(&self) -> QueueTicket {
        let mut t = self.0.tickets();
        let id = t.next;
        t.next += 1;
        t.open.insert(
            id,
            Open {
                final_pass_done: false,
                summary: SummaryHold::Unknown,
            },
        );
        QueueTicket {
            queue: Arc::clone(&self.0),
            id,
        }
    }
}

/// A meeting's place in the queue; ending it (dropping it, a panic's unwind included) lets the next
/// meeting go.
#[derive(Debug)]
pub struct QueueTicket {
    queue: Arc<Shared>,
    id: u64,
}

impl QueueTicket {
    fn wait_until(&self, cancel: &CancelToken, go: impl Fn(&Tickets, u64) -> bool) -> bool {
        let mut t = self.queue.tickets();
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            if go(&t, self.id) {
                return true;
            }
            t = self
                .queue
                .changed
                .wait_timeout(t, WAIT_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Records what this meeting's summary will hold (T12.39), decided from the summary its
    /// summary step will run; newer meetings' live decoding reads it.
    pub fn set_summary(&self, summary: SummaryHold) {
        if let Some(open) = self.queue.tickets().open.get_mut(&self.id) {
            open.summary = summary;
        }
        self.queue.changed.notify_all();
    }

    /// Waits until every older ticket has ended; `false` when `cancel` came first.
    pub fn wait_turn(&self, cancel: &CancelToken) -> bool {
        self.wait_until(cancel, |t, id| t.open.range(..id).next().is_none())
    }

    /// Waits until every older ticket's final pass is over; `false` when `cancel` came first.
    pub fn wait_final_passes(&self, cancel: &CancelToken) -> bool {
        self.wait_until(cancel, |t, id| {
            t.open.range(..id).all(|(_, o)| o.final_pass_done)
        })
    }

    /// Waits until live work needing `needs` may start on `machine`: every older final pass over,
    /// and every older ticket ended or holding a summary it may run beside ([`may_run_beside`]);
    /// `false` when `cancel` came first.
    pub fn wait_live_turn(
        &self,
        needs: &LiveNeeds,
        machine: &Machine,
        cancel: &CancelToken,
    ) -> bool {
        self.wait_until(cancel, |t, id| Self::live_free(t, id, needs, machine))
    }

    fn live_free(t: &Tickets, id: u64, needs: &LiveNeeds, machine: &Machine) -> bool {
        t.open
            .range(..id)
            .all(|(_, o)| o.final_pass_done && may_run_beside(o.summary, needs, machine))
    }

    /// Whether an older meeting still works.
    #[must_use]
    pub fn behind_older_work(&self) -> bool {
        self.queue.tickets().open.range(..self.id).next().is_some()
    }

    /// Whether live work needing `needs` would wait for an older meeting now
    /// ([`Self::wait_live_turn`]).
    #[must_use]
    pub fn live_held(&self, needs: &LiveNeeds, machine: &Machine) -> bool {
        !Self::live_free(&self.queue.tickets(), self.id, needs, machine)
    }

    /// This meeting's final pass is over, whatever its result: newer meetings' speaker work may
    /// start.
    pub fn final_pass_done(&self) {
        if let Some(open) = self.queue.tickets().open.get_mut(&self.id) {
            open.final_pass_done = true;
        }
        self.queue.changed.notify_all();
    }

    /// Ends this meeting's place now, as dropping the last handle does, even while a thread that
    /// was given up still holds a handle (T9.22: a load or step that never returns must not hold
    /// the meetings after it). Ending it twice, or dropping it after, changes nothing.
    pub fn end(&self) {
        self.queue.tickets().open.remove(&self.id);
        self.queue.changed.notify_all();
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.end();
    }
}

/// What a meeting's live work needs, once known: `None` while it cannot be told without waiting
/// (the CUDA start has not decided) or could not be read, which waits as before T12.39.
pub type NeedsFn = Box<dyn Fn() -> Option<LiveNeeds> + Send + Sync>;

/// What lets a [`MeetingHold`] start live decoding beside an older summary (T12.39).
pub struct Beside {
    /// What the meeting's live work needs. Cheap, never loads a model: [`HoldGates::reason`] calls
    /// it on the session thread.
    pub needs: NeedsFn,
    /// This computer.
    pub machine: Machine,
}

impl std::fmt::Debug for Beside {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Beside")
            .field("machine", &self.machine)
            .finish_non_exhaustive()
    }
}

/// The gates of a meeting in the app: its turn in the queue and the CUDA start's decision.
#[derive(Debug)]
pub struct MeetingHold {
    ticket: Arc<QueueTicket>,
    beside: Option<Beside>,
}

impl MeetingHold {
    /// The gates of the meeting holding `ticket`: live decoding waits for every older ticket to
    /// end.
    #[must_use]
    pub fn new(ticket: Arc<QueueTicket>) -> Self {
        Self {
            ticket,
            beside: None,
        }
    }

    /// The gates of the meeting holding `ticket`, whose live decoding may start beside an older
    /// meeting's summary under the rule ([`QueueTicket::wait_live_turn`]).
    #[must_use]
    pub fn beside(ticket: Arc<QueueTicket>, beside: Beside) -> Self {
        Self {
            ticket,
            beside: Some(beside),
        }
    }

    /// What the live work needs, when this hold knows.
    fn needs(&self) -> Option<(LiveNeeds, &Machine)> {
        let b = self.beside.as_ref()?;
        Some(((b.needs)()?, &b.machine))
    }
}

/// Waits until the CUDA start has decided which ONNX Runtime library loads, so what follows reads
/// the order without waiting and never loads the wrong one; `false` when `cancel` came first. It
/// waits at ONNX Runtime's own gate ([`crate::cuda::wait_for_onnx_unless`]), woken when the choice
/// is made, never in a sleep loop of its own.
fn wait_for_the_library_choice(cancel: &CancelToken) -> bool {
    crate::cuda::wait_for_onnx_unless(cancel) && !cancel.is_cancelled()
}

impl HoldGates for MeetingHold {
    fn wait_speakers(&self, cancel: &CancelToken) -> bool {
        self.ticket.wait_final_passes(cancel) && wait_for_the_library_choice(cancel)
    }

    fn wait_transcription(&self, cancel: &CancelToken) -> bool {
        if self.beside.is_none() {
            return self.ticket.wait_turn(cancel) && wait_for_the_library_choice(cancel);
        }
        // The needs read the order, which the CUDA start decides: read them after it.
        if !(self.ticket.wait_final_passes(cancel) && wait_for_the_library_choice(cancel)) {
            return false;
        }
        match self.needs() {
            Some((needs, machine)) => self.ticket.wait_live_turn(&needs, machine, cancel),
            None => self.ticket.wait_turn(cancel),
        }
    }

    fn reason(&self) -> Option<HoldReason> {
        let held = match self.needs() {
            Some((needs, machine)) => self.ticket.live_held(&needs, machine),
            None => self.ticket.behind_older_work(),
        };
        if held {
            Some(HoldReason::PreviousMeeting)
        } else if crate::cuda::onnx_held() {
            Some(HoldReason::Starting)
        } else {
            None
        }
    }
}

#[cfg(test)]
#[path = "meeting_queue_tests.rs"]
mod tests;
