//! Evaluation-only items of this crate that no shipped code may use
//! (`cargo test -p earshot-onnx --test shipped_scan`).

use std::path::{Path, PathBuf};

/// Every `.rs` file under the `src` folder of each workspace crate and of the desktop app,
/// except the test-support crate (examples, tests and xtask are not shipped).
fn shipped_sources() -> Vec<PathBuf> {
    fn scan(dir: &Path, found: &mut Vec<PathBuf>) {
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
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
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
        found.iter().any(|p| p.ends_with("parakeet.rs")),
        "the scan must reach this crate's own sources"
    );
    found
}

/// Whether a shipped file's `text` uses biased decoding: outside `parakeet.rs` (`own`) any
/// mention of either name, as a method, a path or a trait; inside it a call in either form
/// (its definitions and doc links stay).
fn uses_biased_decoding(text: &str, own: bool) -> bool {
    let names = [concat!("Token", "Bias"), concat!("transcribe_", "biased")];
    let calls = [
        concat!(".transcribe_", "biased("),
        concat!("::transcribe_", "biased("),
    ];
    if own {
        calls.iter().any(|c| text.contains(c))
    } else {
        names.iter().any(|n| text.contains(n))
    }
}

/// The scan's rule catches method and UFCS calls (negative control), and passes the defining
/// file's own definitions.
#[test]
fn parakeet_token_bias_scan_catches_method_and_ufcs_calls() {
    let method = concat!("asr.transcribe_", "biased(&a, &o, &c, &mut [])");
    let ufcs = concat!(
        "ParakeetTdt::transcribe_",
        "biased(&mut asr, &a, &o, &c, &mut [])"
    );
    let trait_ufcs = concat!("<B as Token", "Bias>::bias(&mut b, 1)");
    for (text, own) in [
        (method, true),
        (ufcs, true),
        (method, false),
        (ufcs, false),
        (trait_ufcs, false),
    ] {
        assert!(uses_biased_decoding(text, own), "{text} (own {own})");
    }
    let defining = concat!(
        "pub trait Token",
        "Bias {}\n/// [`ParakeetTdt::transcribe_",
        "biased`]\npub fn transcribe_",
        "biased(\n"
    );
    assert!(!uses_biased_decoding(defining, true));
    assert!(!uses_biased_decoding("asr.transcribe(&a, &o, &c)", false));
}

/// T12.15b: biased decoding is evaluation only (`vocab_eval`): no shipped file but the one that
/// defines it names it, and that one never calls it ([`uses_biased_decoding`]).
#[test]
fn parakeet_token_bias_is_used_by_no_shipped_code() {
    let found: Vec<_> = shipped_sources()
        .into_iter()
        .filter(|path| {
            let text = std::fs::read_to_string(path).unwrap();
            uses_biased_decoding(&text, path.ends_with("earshot-onnx/src/parakeet.rs"))
        })
        .collect();
    assert!(
        found.is_empty(),
        "shipped code uses biased decoding: {found:?}"
    );
}
