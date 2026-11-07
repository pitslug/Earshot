//! T6.19 accept: a schema-constrained generation from a GGUF model (the shipped Qwen 3.5 4B
//! since PLAN D22; the Qwen2.5 1.5B spike model until then) that parses and
//! validates as `SummaryV1` (shape, non-blank text, grounding against the meeting's segment
//! ids), on the backend the build asks for, proven from llama.cpp's own load log.

mod common;

use earshot_llm_local::Stop;

#[test]
#[ignore = "needs the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-llm-local -- --include-ignored"]
fn local_llm_summary_parses_as_summary_v1() {
    // Checks the backend (common::expected_backend) from llama.cpp's load log.
    let (mut llm, report) = common::load_model();

    let (generation, parsed) = common::summarise(&mut llm);
    eprintln!("{}", common::speed("summary", &report, &generation));
    eprintln!("evidence: {:?}", report.evidence);
    eprintln!("system: {}", earshot_llm_local::system_info());
    eprintln!("reply:\n{}", generation.text);
    assert_eq!(
        generation.stop,
        Stop::EndOfGeneration,
        "the grammar must complete within the budget"
    );
    let summary = parsed.unwrap_or_else(|e| panic!("not a valid SummaryV1: {e}"));
    assert!(!summary.title.trim().is_empty());
    assert!(generation.generated_tokens > 0 && generation.tokens_per_second() > 0.0);

    // Greedy decoding with a fixed grammar is deterministic on one backend.
    let (again, _) = common::summarise(&mut llm);
    eprintln!("{}", common::speed("summary again", &report, &again));
    assert_eq!(
        again.text, generation.text,
        "a second identical request gave a different reply"
    );
}
