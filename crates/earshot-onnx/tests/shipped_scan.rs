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

/// T12.15b: biased decoding is evaluation only (`vocab_eval`): no shipped file but the one that
/// defines it names it, and that one never calls it.
#[test]
fn parakeet_token_bias_is_used_by_no_shipped_code() {
    let names = [concat!("Token", "Bias"), concat!("transcribe_", "biased")];
    let call = concat!(".transcribe_", "biased(");
    let mut found = Vec::new();
    for path in shipped_sources() {
        let text = std::fs::read_to_string(&path).unwrap();
        let own = path.ends_with(Path::new("earshot-onnx/src/parakeet.rs"));
        if (own && text.contains(call)) || (!own && names.iter().any(|n| text.contains(n))) {
            found.push(path);
        }
    }
    assert!(
        found.is_empty(),
        "shipped code uses biased decoding: {found:?}"
    );
}
