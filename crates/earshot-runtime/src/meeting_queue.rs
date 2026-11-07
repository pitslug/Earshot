//! The queue of meetings (T9.1l): recording starts at once, the heavy work takes turns.
//!
//! A host takes one [`QueueTicket`] per meeting as the meeting starts, in start order. A meeting's
//! heavy work (live decoding, live summaries, then its final pass and summary) runs only when
//! every older ticket has ended ([`QueueTicket::wait_turn`]), so older jobs and held work run
//! first-in, first-out and one summary model runs at a time. Live speaker work waits only for
//! older final passes ([`QueueTicket::wait_final_passes`]), not for a summary, so a long summary
//! does not push the meeting past `SPEAKER_MAX_BUFFER`.
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

use crate::live::{HoldGates, HoldReason};

/// How often a wait looks at its cancel token and the CUDA start.
const WAIT_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
struct Tickets {
    next: u64,
    /// The tickets not yet ended, with whether their final pass is over.
    open: BTreeMap<u64, bool>,
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

    /// The next meeting's place: behind every ticket not yet ended.
    #[must_use]
    pub fn enter(&self) -> QueueTicket {
        let mut t = self.0.tickets();
        let id = t.next;
        t.next += 1;
        t.open.insert(id, false);
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

    /// Waits until every older ticket has ended; `false` when `cancel` came first.
    pub fn wait_turn(&self, cancel: &CancelToken) -> bool {
        self.wait_until(cancel, |t, id| t.open.range(..id).next().is_none())
    }

    /// Waits until every older ticket's final pass is over; `false` when `cancel` came first.
    pub fn wait_final_passes(&self, cancel: &CancelToken) -> bool {
        self.wait_until(cancel, |t, id| t.open.range(..id).all(|(_, done)| *done))
    }

    /// Whether an older meeting still works.
    #[must_use]
    pub fn behind_older_work(&self) -> bool {
        self.queue.tickets().open.range(..self.id).next().is_some()
    }

    /// This meeting's final pass is over, whatever its result: newer meetings' speaker work may
    /// start.
    pub fn final_pass_done(&self) {
        if let Some(done) = self.queue.tickets().open.get_mut(&self.id) {
            *done = true;
        }
        self.queue.changed.notify_all();
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.queue.tickets().open.remove(&self.id);
        self.queue.changed.notify_all();
    }
}

/// The gates of a meeting in the app: its turn in the queue and the CUDA start's decision.
#[derive(Debug)]
pub struct MeetingHold(Arc<QueueTicket>);

impl MeetingHold {
    /// The gates of the meeting holding `ticket`.
    #[must_use]
    pub fn new(ticket: Arc<QueueTicket>) -> Self {
        Self(ticket)
    }
}

/// Waits until the CUDA start has decided which ONNX Runtime library loads, so what follows reads
/// the order without waiting and never loads the wrong one; `false` when `cancel` came first.
fn wait_for_the_library_choice(cancel: &CancelToken) -> bool {
    while crate::cuda::onnx_held() {
        if cancel.is_cancelled() {
            return false;
        }
        std::thread::sleep(WAIT_POLL);
    }
    !cancel.is_cancelled()
}

impl HoldGates for MeetingHold {
    fn wait_speakers(&self, cancel: &CancelToken) -> bool {
        self.0.wait_final_passes(cancel) && wait_for_the_library_choice(cancel)
    }

    fn wait_transcription(&self, cancel: &CancelToken) -> bool {
        self.0.wait_turn(cancel) && wait_for_the_library_choice(cancel)
    }

    fn reason(&self) -> Option<HoldReason> {
        if self.0.behind_older_work() {
            Some(HoldReason::PreviousMeeting)
        } else if crate::cuda::onnx_held() {
            Some(HoldReason::Starting)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_queue_tickets_take_turns_in_start_order() {
        let q = MeetingQueue::new();
        let cancel = CancelToken::new();
        let a = q.enter();
        let b = q.enter();
        let c = q.enter();
        assert!(!a.behind_older_work());
        assert!(a.wait_turn(&cancel));
        assert!(b.behind_older_work());

        // b's turn comes only when a has ended, c's only when both have.
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn({
            let cancel = cancel.clone();
            move || {
                let went = b.wait_turn(&cancel);
                tx.send(went).unwrap();
                drop(b);
            }
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(a);
        assert!(rx.recv_timeout(Duration::from_secs(60)).unwrap());
        waiting.join().unwrap();
        assert!(c.wait_turn(&cancel));
        assert!(!c.behind_older_work());
    }

    #[test]
    fn meeting_queue_speaker_work_waits_only_for_older_final_passes() {
        let q = MeetingQueue::new();
        let a = q.enter();
        let b = q.enter();
        let c = q.enter();
        let stop = CancelToken::new();
        stop.cancel();
        assert!(!c.wait_final_passes(&stop), "a's final pass still runs");
        let go = CancelToken::new();
        a.final_pass_done();
        assert!(
            !c.wait_final_passes(&stop),
            "b's final pass is still to run"
        );
        b.final_pass_done();
        assert!(c.wait_final_passes(&go));
        assert!(!c.wait_turn(&stop), "their summaries still run");
    }

    #[test]
    fn meeting_queue_a_cancelled_wait_returns_and_a_panic_ends_the_ticket() {
        let q = MeetingQueue::new();
        let older = q.enter();
        let newer = q.enter();
        let cancel = CancelToken::new();
        let waiter = std::thread::spawn({
            let cancel = cancel.clone();
            move || newer.wait_turn(&cancel)
        });
        cancel.cancel();
        assert!(!waiter.join().unwrap());
        // `newer` ended with the thread, and a ticket lost in a panic ends too.
        let panicked = std::thread::spawn(move || {
            let _ticket = older;
            panic!("a post-meeting bug");
        });
        assert!(panicked.join().is_err());
        assert!(!q.enter().behind_older_work());
    }
}
