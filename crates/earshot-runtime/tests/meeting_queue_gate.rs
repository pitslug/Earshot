//! T12.30: a held meeting waits for the CUDA start at ONNX Runtime's gate (woken when the choice is
//! made), never in a sleep loop of its own, and still gives way to a cancel. Its own binary,
//! because the gate is one per process.

use std::sync::Arc;
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_runtime::cuda::{callers_slept_at_the_onnx_gate, hold_onnx, onnx_held};
use earshot_runtime::live::HoldGates;
use earshot_runtime::meeting_queue::{MeetingHold, MeetingQueue};

fn until(what: &str, done: impl Fn() -> bool) {
    let limit = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < limit, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn meeting_queue_held_meeting_waits_at_the_onnx_gate_and_gives_way_to_a_cancel() {
    let queue = MeetingQueue::new();
    // No hold: the gates open at once and nobody sleeps at the gate.
    let free = MeetingHold::new(Arc::new(queue.enter()));
    assert!(free.wait_transcription(&CancelToken::new()));
    assert!(free.wait_speakers(&CancelToken::new()));
    // A cancelled meeting never passes, gate open or not.
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(!free.wait_transcription(&cancelled));
    drop(free);
    assert_eq!(callers_slept_at_the_onnx_gate(), 0);

    let hold = hold_onnx();
    assert!(onnx_held());
    // A meeting cancelled while the gate is closed gives way.
    let first = Arc::new(MeetingHold::new(Arc::new(queue.enter())));
    let cancel = CancelToken::new();
    let theirs = cancel.clone();
    let waiting = Arc::clone(&first);
    let gives_way = std::thread::spawn(move || waiting.wait_transcription(&theirs));
    until("the meeting waits at the gate", || {
        callers_slept_at_the_onnx_gate() == 1
    });
    assert!(!gives_way.is_finished());
    cancel.cancel();
    until("the cancelled meeting gave way", || gives_way.is_finished());
    assert!(!gives_way.join().unwrap());
    assert!(onnx_held());
    drop(first);
    // The next one waits at the gate too, and passes once the choice is made.
    let second = Arc::new(MeetingHold::new(Arc::new(queue.enter())));
    let waiting = Arc::clone(&second);
    let patient = std::thread::spawn(move || waiting.wait_speakers(&CancelToken::new()));
    until("the second meeting waits at the gate", || {
        callers_slept_at_the_onnx_gate() == 2
    });
    drop(hold);
    until("the second meeting passed", || patient.is_finished());
    assert!(patient.join().unwrap());
}
