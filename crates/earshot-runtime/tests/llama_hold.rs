//! A meeting's pre-flight never waits for the choice of llama.cpp's DLLs (T8.4af). Its own binary
//! because the gate is process-wide.

use earshot_runtime::host::{hold_llama, summary_gpu_while_preparing};

#[test]
fn llama_hold_a_meeting_prepared_during_a_hold_trusts_the_plan_without_calling_llama() {
    let _hold = hold_llama();
    // A llama call here would wait for the hold above, for ever.
    assert!(summary_gpu_while_preparing(true));
    assert!(!summary_gpu_while_preparing(false));
}
