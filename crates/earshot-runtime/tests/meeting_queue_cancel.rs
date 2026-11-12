//! T9.22: a meeting cancelled after its recording never waits for a CUDA start that may not end.
//! Its own binary, because the gate is one per process.

use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_runtime::cuda::{
    callers_slept_at_the_onnx_gate, hold_onnx, onnx_held, wait_for_onnx_unless,
};

fn until(what: &str, done: impl Fn() -> bool) {
    let limit = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < limit, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn meeting_queue_wait_for_the_cuda_start_gives_way_to_a_cancel() {
    // No hold: nothing waits, cancelled or not.
    assert!(wait_for_onnx_unless(&CancelToken::new()));
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(wait_for_onnx_unless(&cancelled));
    assert_eq!(callers_slept_at_the_onnx_gate(), 0);

    let hold = hold_onnx();
    assert!(onnx_held());
    let cancel = CancelToken::new();
    let theirs = cancel.clone();
    let gives_way = std::thread::spawn(move || wait_for_onnx_unless(&theirs));
    until("the first waiter reached the gate", || {
        callers_slept_at_the_onnx_gate() == 1
    });
    let patient = std::thread::spawn(|| wait_for_onnx_unless(&CancelToken::new()));
    until("the second waiter reached the gate", || {
        callers_slept_at_the_onnx_gate() == 2
    });
    assert!(!gives_way.is_finished());
    // The cancel ends one wait, with the gate still closed.
    cancel.cancel();
    until("the cancelled waiter gave way", || gives_way.is_finished());
    assert!(!gives_way.join().unwrap());
    assert!(onnx_held());
    assert!(!patient.is_finished());
    // The other waits on, and passes once the choice is made.
    drop(hold);
    until("the other waiter passed", || patient.is_finished());
    assert!(patient.join().unwrap());
}
