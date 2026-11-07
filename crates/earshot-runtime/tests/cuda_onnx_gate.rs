//! T8.4af: while the app chooses ONNX Runtime's library, nothing loads one. Its own binary, because
//! the gate and the library are one per process.

use std::time::{Duration, Instant};

use earshot_runtime::backends::{Accelerator, ModelDir, init_onnx};
use earshot_runtime::cuda::{callers_slept_at_the_onnx_gate, hold_onnx, onnx_held, wait_for_onnx};
use earshot_runtime::host::settled_order;

fn until(what: &str, done: impl Fn() -> bool) {
    let limit = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < limit, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn cuda_onnx_gate_holds_every_load_and_the_order_until_the_choice_is_made() {
    // No hold: nothing waits.
    wait_for_onnx();
    assert_eq!(callers_slept_at_the_onnx_gate(), 0);
    assert!(!settled_order().is_empty());

    let hold = hold_onnx();
    assert!(onnx_held());
    let missing = std::env::temp_dir().join(format!("earshot-onnx-gate-{}", std::process::id()));
    let load = std::thread::spawn(move || {
        init_onnx(&ModelDir::new(missing), Accelerator::Cpu).map(|_| ())
    });
    until("the load reached the gate", || {
        callers_slept_at_the_onnx_gate() == 1
    });
    let order = std::thread::spawn(settled_order);
    until("the order reached the gate", || {
        callers_slept_at_the_onnx_gate() == 2
    });
    assert!(!load.is_finished());
    assert!(!order.is_finished());

    drop(hold);
    assert!(!onnx_held());
    assert!(!order.join().unwrap().is_empty());
    // Past the gate it did what it always does: this folder holds no library.
    assert!(matches!(
        load.join().unwrap(),
        Err(earshot_runtime::RuntimeError::ModelMissing { .. })
    ));

    // A panic on the holder's thread opens the gate.
    let thread = std::thread::spawn(|| {
        let _hold = hold_onnx();
        panic!("the holder's own failure");
    });
    assert!(thread.join().is_err());
    assert!(!onnx_held());
    wait_for_onnx();
}
