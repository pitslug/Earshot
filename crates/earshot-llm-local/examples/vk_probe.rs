//! T8.3i diagnosis probe (no shipped binary uses it): why llama.cpp on Vulkan stalls on the
//! Surface's Adreno. Loads a GGUF, decodes one short chat prompt and a short reply, times
//! the prompt and the reply separately, and prints llama.cpp's log since the start. A
//! watchdog cancels the run after `--timeout` seconds; generation checks the token between
//! prompt batches and before every reply token, so a run that does not return within
//! `GRACE` of the cancel is stuck inside one `llama_decode` (the backend), and the probe
//! prints that verdict and the log, then exits with code 3.
//!
//! cargo run --release -p earshot-llm-local --features vulkan --example vk_probe --
//!     <model.gguf> [--cpu] [--ctx N] [--prompt-words N] [--reply N] [--timeout S]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ChatMessage, Role};
use earshot_llm_local::{LoadOptions, LocalLlm, log};

/// How long after the cancel a cooperative stop must have returned.
const GRACE: Duration = Duration::from_secs(30);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = args
        .first()
        .expect("usage: vk_probe <model.gguf> [options]");
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str, default: u64| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map_or(default, |v| v.parse().expect(name))
    };
    let gpu = !flag("--cpu");
    let ctx = u32::try_from(value("--ctx", 2048)).unwrap();
    let words = value("--prompt-words", 20);
    let reply = usize::try_from(value("--reply", 16)).unwrap();
    let timeout = Duration::from_secs(value("--timeout", 120));

    println!("system info: {}", earshot_llm_local::system_info());
    let mark = log::mark();
    let options = LoadOptions {
        gpu,
        context_tokens: ctx,
        ..LoadOptions::default()
    };
    let (mut llm, report) = LocalLlm::load(std::path::Path::new(model), &options)
        .unwrap_or_else(|e| panic!("load: {e}"));
    println!(
        "loaded {} on {} in {:.1} s, context {}",
        report.description,
        report.backend.name(),
        report.load_time.as_secs_f64(),
        report.context_tokens
    );
    for line in &report.evidence {
        println!("  evidence: {line}");
    }

    let filler: Vec<&str> =
        std::iter::repeat_n("meeting", usize::try_from(words).unwrap()).collect();
    let messages = [ChatMessage::new(
        Role::User,
        format!("Say hello in a few words. {}", filler.join(" ")),
    )];
    let grammar = r"root ::= [a-z ]+";

    let cancel = CancelToken::new();
    let done = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let watchdog = {
        let cancel = cancel.clone();
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut next = Duration::from_secs(10);
            while !done.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
                let t = started.elapsed();
                if t >= next {
                    println!("  ... {:.0} s", t.as_secs_f64());
                    next += Duration::from_secs(10);
                }
                if t >= timeout && !cancel.is_cancelled() {
                    println!("watchdog: cancelling at {:.0} s", t.as_secs_f64());
                    cancel.cancel();
                }
                if t >= timeout + GRACE {
                    println!(
                        "VERDICT: stuck inside one llama_decode (no return {} s after the cancel)",
                        GRACE.as_secs()
                    );
                    print_log(mark);
                    std::process::exit(3);
                }
            }
        })
    };
    let result = llm.generate_cancellable(&messages, "", grammar, reply, &cancel);
    done.store(true, Ordering::SeqCst);
    watchdog.join().unwrap();
    match result {
        Ok(g) => println!(
            "VERDICT: completed. prompt {} tokens in {:.2} s ({:.1} tok/s); reply {} tokens in {:.2} s ({:.2} tok/s), stop {:?}; text {:?}",
            g.prompt_tokens,
            g.prompt_time.as_secs_f64(),
            g.prompt_tokens_per_second(),
            g.generated_tokens,
            g.generate_time.as_secs_f64(),
            g.tokens_per_second(),
            g.stop,
            g.text
        ),
        Err(e) => println!(
            "VERDICT: returned {e} after {:.1} s (cooperative stop: slow, not stuck)",
            started.elapsed().as_secs_f64()
        ),
    }
    print_log(mark);
}

fn print_log(mark: u64) {
    match log::since(mark) {
        Some(lines) => {
            println!("--- llama.cpp log ({} lines) ---", lines.len());
            for l in lines {
                println!("{l}");
            }
        }
        None => println!("--- llama.cpp log overflowed the capture ---"),
    }
}
