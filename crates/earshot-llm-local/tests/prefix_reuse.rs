//! T12.25 part 1: prompt prefix reuse gives the same greedy reply as a fresh prompt, and
//! decodes fewer prompt tokens. A repair round (the original prompt, the reply, one more
//! user turn) shares its whole start with the request before it.

mod common;

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ChatMessage, PromptPlan, Role, plan};
use earshot_llm_local::grammar::{SummaryLimits, summary_grammar};

#[test]
#[ignore = "needs the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-llm-local --test prefix_reuse -- --include-ignored"]
fn prefix_reuse_gives_the_fresh_reply_and_decodes_less() {
    let (mut llm, _report) = common::load_model();
    // A longer meeting than the shared one, so the shared start spans whole micro-batches.
    let mut input = common::meeting();
    let first = input.segments.clone();
    for k in 1..8u64 {
        for s in &first {
            let mut s = s.clone();
            s.id = format!(
                "0193a5b0-1c00-7000-8000-{:012x}",
                k * 100 + s.start.as_millis() / 9_000 + 1
            )
            .parse()
            .unwrap();
            s.start = earshot_core::asr::MeetingTime::from_millis(s.start.as_millis() + k * 90_000);
            input.segments.push(s);
        }
    }
    input.meeting.duration_ms = 8 * 90_000;
    let PromptPlan::Single(prompt) = plan(&input, common::BUDGET).expect("plan") else {
        panic!("the meeting must fit in one request");
    };
    let grammar = summary_grammar(&prompt.cites, SummaryLimits::default()).expect("grammar");
    let cancel = CancelToken::new();
    let run = |llm: &mut earshot_llm_local::LocalLlm, messages: &[ChatMessage]| {
        llm.generate_cancellable(messages, "", &grammar, 400, &cancel)
            .unwrap_or_else(|e| panic!("{e}"))
    };
    // The fresh replies: reuse off.
    let a_fresh = run(&mut llm, &prompt.messages);
    let mut repair = prompt.messages.clone();
    repair.push(ChatMessage::new(Role::Assistant, a_fresh.text.clone()));
    repair.push(ChatMessage::new(
        Role::User,
        "Check your reply against the rules and send it again.",
    ));
    let b_fresh = run(&mut llm, &repair);
    let off = llm.take_reuse_stats();
    assert_eq!((off.requests, off.reused_tokens), (2, 0));
    // The same two requests with reuse on, from a clean context.
    llm.set_prefix_reuse(true);
    llm.forget_prefix();
    let a = run(&mut llm, &prompt.messages);
    let b = run(&mut llm, &repair);
    let on = llm.take_reuse_stats();
    assert_eq!(a.text, a_fresh.text, "the first request starts fresh");
    assert_eq!(
        b.text, b_fresh.text,
        "the repair reply must be byte-identical"
    );
    assert_eq!(on.prompt_tokens, off.prompt_tokens);
    assert!(
        on.reused_tokens >= 512,
        "the repair round shares the whole first prompt: {on:?}"
    );
    eprintln!(
        "prompt tokens {} decoded off, {} on ({} kept)",
        off.decoded_tokens(),
        on.decoded_tokens(),
        on.reused_tokens
    );
}

/// Every `.rs` file under the `src` folder of each workspace crate and of the desktop app,
/// except the test-support crate (examples, tests and xtask are not shipped).
fn shipped_sources() -> Vec<std::path::PathBuf> {
    fn scan(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .flatten();
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut roots = vec![repo.join("apps/desktop/src-tauri/src")];
    for krate in std::fs::read_dir(repo.join("crates")).unwrap().flatten() {
        if krate.file_name() != "earshot-llm-replay" {
            roots.push(krate.path().join("src"));
        }
    }
    let mut found = Vec::new();
    for root in roots.iter().filter(|r| r.is_dir()) {
        scan(root, &mut found);
    }
    assert!(
        found
            .iter()
            .any(|p| p.ends_with(std::path::Path::new("earshot-llm-local/src/lib.rs"))),
        "the scan must reach this crate's own sources"
    );
    found
}

/// T12.25: prefix reuse is evaluation only (`sum_eval --prefix-reuse`) until the owner turns it
/// on: no shipped file calls `set_prefix_reuse`, the one that defines it included.
#[test]
fn prefix_reuse_is_used_by_no_shipped_code() {
    let call = concat!(".set_prefix_", "reuse(");
    let found: Vec<_> = shipped_sources()
        .into_iter()
        .filter(|p| std::fs::read_to_string(p).unwrap().contains(call))
        .collect();
    assert!(
        found.is_empty(),
        "shipped code turns prefix reuse on: {found:?}"
    );
}
