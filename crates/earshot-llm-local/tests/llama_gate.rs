//! While a hold lives, no path into llama.cpp runs (T8.4af). Its own binary because the gate is
//! process-wide. The proof is the gate's own count of who slept there, never a clock: a first
//! llama call can take longer than any wait a test would afford.

use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use earshot_llm_local::{
    LoadOptions, LocalLlm, callers_slept_at_the_gate, hold_llama, llama_held, supports_gpu,
    system_info,
};

#[test]
fn llama_gate_holds_every_way_in_until_it_is_released() {
    let hold = hold_llama();
    assert!(llama_held());
    let (tx, rx) = mpsc::channel::<&'static str>();
    let doors: [(&'static str, fn()); 3] = [
        ("supports_gpu", || {
            let _ = supports_gpu();
        }),
        ("system_info", || {
            let _ = system_info();
        }),
        ("load", || {
            // A file that does not exist: refused by llama.cpp, once it may be asked.
            let _ = LocalLlm::load(Path::new("no-such-model.gguf"), &LoadOptions::default());
        }),
    ];
    let threads: Vec<_> = doors
        .into_iter()
        .map(|(name, door)| {
            let tx = tx.clone();
            thread::spawn(move || {
                door();
                tx.send(name).unwrap();
            })
        })
        .collect();
    drop(tx);
    // Each of the three reaches the gate and sleeps. Only a failing gate runs into this limit.
    let limit = Instant::now() + Duration::from_secs(60);
    while callers_slept_at_the_gate() < 3 {
        assert!(Instant::now() < limit, "a call never reached the gate");
        assert!(rx.try_recv().is_err(), "a call went through a closed gate");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(callers_slept_at_the_gate(), 3);
    assert!(rx.try_recv().is_err(), "a call went through a closed gate");
    drop(hold);
    assert!(!llama_held());
    let mut through: Vec<_> = rx.iter().collect();
    through.sort_unstable();
    assert_eq!(through, ["load", "supports_gpu", "system_info"]);
    assert_eq!(
        callers_slept_at_the_gate(),
        3,
        "an open gate puts nobody to sleep"
    );
    for t in threads {
        t.join().unwrap();
    }
}
