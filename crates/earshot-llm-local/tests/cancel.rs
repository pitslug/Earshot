//! Cooperative cancellation on the real model: a cancelled token stops generation with
//! `LlmError::Cancelled` before any reply token, and an uncancelled token changes nothing.

mod common;

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ChatMessage, Role};
use earshot_llm_local::{LlmError, LoadOptions, LocalLlm};

#[test]
#[ignore = "needs the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-llm-local -- --include-ignored"]
fn cancel_stops_generation_on_the_real_model() {
    let options = LoadOptions {
        gpu: common::gpu_build(),
        context_tokens: 2048,
        ..LoadOptions::default()
    };
    let (mut llm, _) = LocalLlm::load(&common::model_path(common::MODEL, "summary"), &options)
        .unwrap_or_else(|e| panic!("{e}"));
    let messages = [ChatMessage::new(Role::User, "Say yes or no.")];
    let grammar = r#"root ::= "yes" | "no""#;
    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        llm.generate_cancellable(&messages, "", grammar, 8, &cancel),
        Err(LlmError::Cancelled)
    );
    // The same model still answers with a fresh token.
    let g = llm
        .generate_cancellable(&messages, "", grammar, 8, &CancelToken::new())
        .unwrap();
    assert!(g.text == "yes" || g.text == "no", "{:?}", g.text);
}
