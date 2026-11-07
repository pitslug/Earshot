//! Cooperative cancellation on the real model from another thread, during generation: the
//! per-token check stops the reply with `LlmError::Cancelled` well before the reply budget
//! (`cancel.rs` covers a token cancelled before the call).

mod common;

use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ChatMessage, Role};
use earshot_llm_local::{LlmError, LoadOptions, LocalLlm, Stop};

#[test]
#[ignore = "needs the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-llm-local -- --include-ignored"]
fn cancel_from_another_thread_stops_generation_between_tokens() {
    let options = LoadOptions {
        gpu: common::gpu_build(),
        context_tokens: 2048,
        ..LoadOptions::default()
    };
    let (mut llm, _) = LocalLlm::load(&common::model_path(common::MODEL, "summary"), &options)
        .unwrap_or_else(|e| panic!("{e}"));
    let messages = [ChatMessage::new(
        Role::User,
        "Write the letter a many times.",
    )];
    // The grammar never ends (no base case), so the reply runs to the budget: no
    // end-of-generation token can stop it before the cancel does. The budget is not a
    // multiple of the loop guard's check interval (`LOOP_CHECK_TOKENS`, 32): with Qwen 3.5 4B
    // (PLAN D22; Qwen2.5 1.5B before, at 160 tokens) the reply is many letters per token, and
    // at 160 tokens the guard stopped it as `Stop::Looping` (measured 2026-11-02, CPU); at
    // its last check before 150 (128 tokens) it had not.
    let grammar = r"root ::= [a-z] root";
    const BUDGET: usize = 150;

    // Uncancelled: the whole budget, and how long it takes.
    let started = Instant::now();
    let full = llm
        .generate_cancellable(&messages, "", grammar, BUDGET, &CancelToken::new())
        .unwrap_or_else(|e| panic!("{e}"));
    let full_time = started.elapsed();
    assert_eq!(full.stop, Stop::MaxTokens, "{full:?}");
    assert_eq!(full.generated_tokens, BUDGET);

    // Cancelled from another thread halfway through the reply (after the prompt).
    let cancel = CancelToken::new();
    let at = full.prompt_time + full.generate_time / 2;
    let started = Instant::now();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(at);
            cancel.cancel();
            Instant::now()
        })
    };
    let result = llm.generate_cancellable(&messages, "", grammar, BUDGET, &cancel);
    let returned = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert_eq!(result, Err(LlmError::Cancelled));
    // It stopped early: before the uncancelled run's time, and within a few tokens of the
    // cancel (one token's time from the measured run, plus a fixed allowance). Both are
    // wall-clock margins that can flap on a loaded host: re-run alone before calling a
    // failure environmental, and never widen them.
    let per_token = full.generate_time / u32::try_from(BUDGET).unwrap();
    let after_cancel = returned.saturating_duration_since(cancelled_at);
    assert!(
        started.elapsed() < full_time,
        "cancelled run {:?} vs full {full_time:?}",
        started.elapsed()
    );
    assert!(
        after_cancel < per_token * 4 + Duration::from_millis(500),
        "returned {after_cancel:?} after the cancel; one token takes {per_token:?}"
    );
}
